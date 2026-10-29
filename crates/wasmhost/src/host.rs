use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use wasmer::{
    Engine, Function, FunctionEnv, FunctionEnvMut, Instance, Memory, Module,
    RuntimeError, Store, TypedFunction,
};

use crate::sandbox::{Frame, InputEvent, Sandbox};
use crate::wasi;
use crate::Error;

/// Shared `Engine` (JIT compiler + artifact cache) so multiple live plugin
/// instances reuse the same compiled artifact instead of re-JITing.
static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();

fn engine() -> Engine {
    ENGINE.get_or_init(Engine::default).clone()
}

/// Per-instance data handed to every host function of one instance.
///
/// `mem` is filled after instantiation (the plugin's exported linear memory);
/// the handlers resolve it lazily so they stay plain `fn` items (wasmer host
/// functions must not capture).
#[derive(Clone)]
pub(crate) struct HsEnv {
    pub sbx: Arc<Sandbox>,
    pub mem: Arc<Mutex<Option<Memory>>>,
    pub wasi_rng_seed: Arc<Mutex<u64>>,
}

pub(crate) fn errno(e: i32) -> Result<Vec<wasmer::Value>, RuntimeError> {
    Ok(vec![wasmer::Value::I32(e)])
}

pub(crate) fn memview<'e>(env: &'e FunctionEnvMut<HsEnv>) -> Option<(Memory, wasmer::MemoryView<'e>)> {
    let mem = env.data().mem.lock().unwrap().clone()?;
    // `FunctionEnvMut` implements `AsStoreRef`, so the view's lifetime is
    // tied to `env` (which outlives this frame in every handler).
    let view = mem.view(env);
    Some((mem, view))
}

/// Everything the plugin thread owns for one live instance. Not `Send` —
/// wasmer's `Store`/`Instance` are confined to the thread that created them;
/// that is what lets one plugin thread drive one instance with no locks
/// inside the runtime.
struct WasmRunner {
    store: Store,
    #[allow(dead_code)] // kept alive: the host `Function`s share its `FunctionEnv`
    env: FunctionEnv<HsEnv>,
    #[allow(dead_code)] // kept alive for the lifetime of the function handles
    instance: Instance,
    f_init: TypedFunction<(u32, u32, u32), u32>,
    f_tick: TypedFunction<i64, u32>,
    f_destroy: TypedFunction<(), u32>,
}

fn wasmer_err(e: impl std::fmt::Display) -> Error {
    Error::Wasm(e.to_string())
}

impl WasmRunner {
    /// Compile + instantiate `bytes` with the sandbox import set and resolve
    /// the mandatory plugin exports (`init` / `tick` / `destroy`).
    fn start(sandbox: Arc<Sandbox>, bytes: &[u8]) -> Result<Self, Error> {
        let mem_slot = Arc::new(Mutex::new(None));
        let hs_env = HsEnv {
            sbx: sandbox.clone(),
            mem: mem_slot.clone(),
            wasi_rng_seed: Arc::new(Mutex::new(0x9e37_79b9_7f4a_7c15)),
        };
        let mut store = Store::new(engine());
        let module = Module::new(&store, &bytes).map_err(wasmer_err)?;
        let env = FunctionEnv::new(&mut store, hs_env.clone());
        let wasi_ns = wasi::wasi_namespace(&mut store, &env);
        let imports = wasmer::imports! {
            "x" => {
                "x_log" => Function::new_typed_with_env(&mut store, &env, x_log),
                "x_now_ms" => Function::new_typed_with_env(&mut store, &env, x_now_ms),
                "x_frame_commit" => Function::new_typed_with_env(&mut store, &env, x_frame_commit),
                "x_input_poll" => Function::new_typed_with_env(&mut store, &env, x_input_poll),
            },
            "wasi_snapshot_preview1" => wasi_ns,
        };
        let instance = Instance::new(&mut store, &module, &imports).map_err(wasmer_err)?;

        let exports = &instance.exports;
        let f_init = exports
            .get_typed_function::<(u32, u32, u32), u32>(&store, "init")
            .map_err(|_| Error::MissingExport("init".into()))?;
        let f_tick = exports
            .get_typed_function::<i64, u32>(&store, "tick")
            .map_err(|_| Error::MissingExport("tick".into()))?;
        let f_destroy = exports
            .get_typed_function::<(), u32>(&store, "destroy")
            .map_err(|_| Error::MissingExport("destroy".into()))?;

        let mem = exports.get_memory("memory").map_err(wasmer_err)?.clone();
        *mem_slot.lock().unwrap() = Some(mem);

        Ok(Self {
            store,
            env,
            instance,
            f_init,
            f_tick,
            f_destroy,
        })
    }

    fn init(&mut self, width: u32, height: u32, dpr_x10: u32) -> Result<(), Error> {
        let r = self
            .f_init
            .call(&mut self.store, width, height, dpr_x10)
            .map_err(wasmer_err)?;
        if r != 0 {
            return Err(Error::PluginDeclined { code: r });
        }
        Ok(())
    }

    fn tick(&mut self, dt_ms: i64) -> Result<(), Error> {
        let r = self.f_tick.call(&mut self.store, dt_ms).map_err(wasmer_err)?;
        if r != 0 {
            return Err(Error::PluginDeclined { code: r });
        }
        Ok(())
    }

    fn destroy(&mut self) {
        let _ = self.f_destroy.call(&mut self.store).map_err(wasmer_err);
    }
}

// ---------------------------------------------------------------------------
// `x` namespace — the plugin → host ABI (see x/README.md)
// ---------------------------------------------------------------------------

fn read_wasm_bytes(
    env: &FunctionEnvMut<HsEnv>,
    ptr: u32,
    len: u32,
) -> Result<Vec<u8>, RuntimeError> {
    let Some((_, view)) = memview(env) else {
        return Err(RuntimeError::new("memory not wired"));
    };
    let mut buf = vec![0u8; len as usize];
    view.read(ptr as u64, &mut buf)
        .map_err(|_| RuntimeError::new("OOB read"))?;
    Ok(buf)
}

/// `x_log(ptr, len) -> errno` — plugin log line → host log (chatx.log).
fn x_log(env: FunctionEnvMut<HsEnv>, ptr: u32, len: u32) -> u32 {
    let sbx = env.data().sbx.clone();
    match read_wasm_bytes(&env, ptr, len) {
        Ok(buf) => {
            sbx.log(&String::from_utf8_lossy(&buf));
            0
        }
        Err(_) => 73, // ENOBUFS
    }
}

/// `x_now_ms() -> i64` — host monotonic clock, ms.
fn x_now_ms(_env: FunctionEnvMut<HsEnv>) -> i64 {
    crate::monotonic_ms()
}

/// `x_frame_commit(ptr, width, height, stride) -> errno`
///
/// The plugin draws RGBA8 (straight alpha) into its linear memory and
/// commits it; the host copies the frame out and parks it in the sandbox
/// for the UI pump to pick up.
fn x_frame_commit(env: FunctionEnvMut<HsEnv>, ptr: u32, width: u32, height: u32, stride: u32) -> u32 {
    if width == 0 || height == 0 {
        return 28;
    }
    let stride = stride.max(width * 4);
    let max_frame_bytes: u64 = 256 * 1024 * 1024;
    let total = (stride as u64) * (height as u64);
    if total > max_frame_bytes {
        return 12; // ENOMEM
    }
    let Some((_, view)) = memview(&env) else {
        return 63;
    };
    let mut rgba = vec![0u8; total as usize];
    if view.read(ptr as u64, &mut rgba).is_err() {
        return 73;
    }
    env.data().sbx.store_frame(Frame {
        rgba,
        width,
        height,
    });
    0
}

/// `x_input_poll(kind*, x*, y*, code*) -> 0|1`
///
/// Drains one host-pushed event into the four pointed-to u32s.
fn x_input_poll(env: FunctionEnvMut<HsEnv>, kind_p: u32, x_p: u32, y_p: u32, code_p: u32) -> i32 {
    let ev = match env.data().sbx.pop_input() {
        Some(ev) => ev,
        None => return 0,
    };
    let Some((_, view)) = memview(&env) else {
        return 0;
    };
    let ok = view
        .write(kind_p as u64, &ev.kind.to_le_bytes())
        .and_then(|_| view.write(x_p as u64, &ev.x.to_le_bytes()))
        .and_then(|_| view.write(y_p as u64, &ev.y.to_le_bytes()))
        .and_then(|_| view.write(code_p as u64, &ev.code.to_le_bytes()));
    match ok {
        Ok(()) => 1,
        Err(_) => 73,
    }
}

// ---------------------------------------------------------------------------
// Instance lifecycle — one dedicated thread per instance
// ---------------------------------------------------------------------------

/// A running plugin instance: plugin thread + sandbox channels.
pub struct PluginInstance {
    pub sandbox: Arc<Sandbox>,
    stop: Arc<AtomicBool>,
    join: Mutex<Option<std::thread::JoinHandle<Result<(), Error>>>>,
    pub(crate) settled: AtomicBool,
    pub(crate) settled_error: Mutex<Option<String>>,
}

/// Start one plugin instance.
///
/// `init(width, height, dpr_x10)` runs on the plugin thread before the first
/// `tick`; `width`/`height` are canvas pixels, `dpr_x10` is the effective
/// device-pixel-ratio × 10. Ticks continue until [`PluginInstance::close`].
pub fn open(
    app_id: &str,
    wasm_bytes: Vec<u8>,
    width: u32,
    height: u32,
    dpr_x10: u32,
) -> Result<Arc<PluginInstance>, Error> {
    let sandbox = Sandbox::with_tracing_log(app_id);
    let inst = Arc::new(PluginInstance {
        sandbox: sandbox.clone(),
        stop: Arc::new(AtomicBool::new(false)),
        join: Mutex::new(None),
        settled: AtomicBool::new(false),
        settled_error: Mutex::new(None),
    });

    let stop = inst.stop.clone();
    let inst_ref = inst.clone();
    let app_id_for_log = app_id.to_string();
    let handle = std::thread::Builder::new()
        .name(format!("x-app::{app_id}"))
        .spawn(move || {
            let result =
                run_plugin_thread(sandbox, wasm_bytes, width, height, dpr_x10, &stop);
            inst_ref.settled.store(true, Ordering::SeqCst);
            if let Err(e) = &result {
                *inst_ref.settled_error.lock().unwrap() = Some(e.to_string());
                tracing::error!("[x] {app_id_for_log}: plugin thread failed: {e}");
            }
            result
        })
        .map_err(|e| Error::other(format!("plugin thread spawn: {e}")))?;
    *inst.join.lock().unwrap() = Some(handle);
    Ok(inst)
}

/// ~60 fps tick cadence, sleeping in small slices so `close()` is honored
/// within a few frames.
const TICK_TARGET: Duration = Duration::from_millis(16);
const TICK_SLICE: Duration = Duration::from_millis(4);

fn run_plugin_thread(
    sandbox: Arc<Sandbox>,
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    dpr_x10: u32,
    stop: &AtomicBool,
) -> Result<(), Error> {
    let app_id = sandbox.app_id.clone();
    let mut runner = WasmRunner::start(sandbox.clone(), &bytes)?;
    tracing::info!("[x] {app_id}: instance started at {width}×{height} dpr×{dpr_x10}");
    runner.init(width, height, dpr_x10)?;

    let mut last = Instant::now();
    loop {
        let now = Instant::now();
        let dt_ms = now.duration_since(last).as_millis() as i64;
        last = now;
        if let Err(e) = runner.tick(dt_ms) {
            tracing::warn!("[x] {app_id}: tick failed ({e}); stopping instance");
            break;
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let elapsed = now.elapsed();
        if elapsed < TICK_SLICE {
            std::thread::sleep(TICK_SLICE - elapsed);
        }
        std::thread::sleep(TICK_TARGET);
    }

    runner.destroy();
    if let Some(code) = sandbox.exited_code() {
        tracing::info!("[x] {app_id}: plugin exited with code {code}");
        return Err(Error::PluginExited { code });
    }
    tracing::info!("[x] {app_id}: instance destroyed");
    Ok(())
}

impl PluginInstance {
    /// Push an input event for the plugin to drain on its next `tick`.
    pub fn push_input(&self, ev: InputEvent) {
        self.sandbox.push_input(ev);
    }

    /// Take the latest committed frame (if any).
    pub fn take_frame(&self) -> Option<Frame> {
        self.sandbox.take_frame()
    }

    /// Ask the plugin to shut down (at the next tick boundary → `destroy`)
    /// and wait for the thread to join. Idempotent.
    pub fn close(&self) -> Result<(), Error> {
        self.stop.store(true, Ordering::SeqCst);
        let handle = self.join.lock().unwrap().take();
        if let Some(h) = handle {
            // The plugin thread sleeps in ≤ 20 ms slices, so this join is
            // bounded by one tick cycle + destroy.
            h.join().map_err(|_| Error::JoinPanic)?
        } else if let Some(msg) = self.settled_error.lock().unwrap().clone() {
            Err(Error::Wasm(msg))
        } else {
            Ok(())
        }
    }

    /// True once the plugin thread has finished (success or failure).
    pub fn settled(&self) -> bool {
        self.settled.load(Ordering::SeqCst)
    }

    /// The start/shutdown error, if the thread set one.
    pub fn settled_error(&self) -> Option<String> {
        self.settled_error.lock().unwrap().clone()
    }
}

/// Monotonic clock in ms (shared with import handlers).
pub fn monotonic_ms() -> i64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as i64
}

/// Monotonic clock in ns.
pub fn monotonic_ns() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}
