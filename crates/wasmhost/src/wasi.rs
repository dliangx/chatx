//! Minimal WASI preview1 (`wasi_snapshot_preview1`) surface.
//!
//! The plugins target `wasm32-wasip1` and link Rust `std`, so the runtime
//! needs the handful of import entries `std` references at link time. We
//! implement the small useful subset (stdout/stderr logging, clocks, a
//! weak source of randomness, process termination) and answer everything
//! else with `ENOSYS` — all plugin I/O is supposed to go through the `x`
//! namespace anyway (see x/README.md), never through WASI files/sockets.

use wasmer::{Function, FunctionEnv, FunctionEnvMut, FunctionType, Type, Value};

use crate::host::{errno, memview, monotonic_ns, HsEnv};

fn ty(params: usize) -> FunctionType {
    FunctionType::new(vec![Type::I32; params], vec![Type::I32])
}

fn log_wasi(env: &FunctionEnvMut<HsEnv>, stream: &[u8]) {
    if !stream.is_empty() {
        env.data().sbx.log(&String::from_utf8_lossy(&stream));
    }
}

fn h_args_sizes_get(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    if let Some((_, view)) = memview(&env) {
        let _ = view.write(args[0].unwrap_i32() as u32 as u64, &0u32.to_le_bytes());
        let _ = view.write(args[1].unwrap_i32() as u32 as u64, &0u32.to_le_bytes());
    }
    errno(0)
}

fn h_args_get(_env: FunctionEnvMut<HsEnv>, _args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    errno(0)
}

fn h_environ_sizes_get(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    if let Some((_, view)) = memview(&env) {
        let _ = view.write(args[0].unwrap_i32() as u32 as u64, &0u32.to_le_bytes());
        let _ = view.write(args[1].unwrap_i32() as u32 as u64, &0u32.to_le_bytes());
    }
    errno(0)
}

fn h_environ_get(_env: FunctionEnvMut<HsEnv>, _args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    errno(0)
}

/// `fd_write(fd, iovs, iovs_len, nwritten*)` — std/`println!`/panic output
/// lands here on fds 1 and 2; the sandbox log sink forwards it to chatx.log.
fn h_fd_write(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    let _fd = args[0].unwrap_i32();
    let iovs = args[1].unwrap_i32() as u32 as u64;
    let n = args[2].unwrap_i32() as u32 as u64;
    let nwritten_p = args[3].unwrap_i32() as u32 as u64;
    let Some((_, view)) = memview(&env) else {
        return errno(63);
    };
    let mut out: Vec<u8> = Vec::new();
    for i in 0..n {
        let mut w = [0u8; 8];
        view
            .read(iovs + i * 8, &mut w)
            .map_err(|_| wasmer::RuntimeError::new("fd_write: iovs OOB"))?;
        let ptr = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) as u64;
        let len = u32::from_le_bytes([w[4], w[5], w[6], w[7]]) as usize;
        let mut buf = vec![0u8; len];
        view.read(ptr, &mut buf).map_err(|_| wasmer::RuntimeError::new("fd_write: buf OOB"))?;
        out.extend_from_slice(&buf);
    }
    log_wasi(&env, &out);
    let _ = view.write(nwritten_p, &(out.len() as u32).to_le_bytes());
    errno(0)
}

fn h_fd_close(_env: FunctionEnvMut<HsEnv>, _args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    errno(0)
}

fn h_fd_fdstat_get(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    if let Some((_, view)) = memview(&env) {
        // struct fdstat: fd_type(u8)=1 regular file, flags(u16), rights×2(u64)
        let mut stat = [0u8; 24];
        stat[0] = 1;
        let _ = view.write(args[1].unwrap_i32() as u32 as u64, &stat);
    }
    errno(0)
}

fn h_sched_yield(_env: FunctionEnvMut<HsEnv>, _args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    errno(0)
}

fn h_clock_res_get(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    if let Some((_, view)) = memview(&env) {
        let _ = view.write(args[1].unwrap_i32() as u32 as u64, &1u64.to_le_bytes());
    }
    errno(0)
}

fn h_clock_time_get(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    let id = args[0].unwrap_i32() as u32;
    let _precision = args[1].unwrap_i64(); // wasip1: inline i64 timestamp precision
    let out_ptr = args[2].unwrap_i32() as u32 as u64;
    let Some((_, view)) = memview(&env) else {
        return errno(63);
    };
    let ns: u64 = match id {
        0 => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64),
        _ => monotonic_ns(),
    };
    let _ = view.write(out_ptr, &ns.to_le_bytes());
    errno(0)
}

/// Weak xorshift64* PRNG — good enough for `rand`/`getrandom` shims inside a
/// sandbox (not for secrets).
fn h_random_get(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    let ptr = args[0].unwrap_i32() as u32 as u64;
    let len = args[1].unwrap_i32() as u32 as usize;
    let Some((_, view)) = memview(&env) else {
        return errno(63);
    };
    let seed = {
        let mut g = env.data().wasi_rng_seed.lock().unwrap();
        let ns = monotonic_ns();
        let s = *g ^ ns;
        *g = s.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(ns);
        if *g == 0 {
            *g = 0x9e37_79b9_7f4a_7c15
        }
        *g
    };
    let mut out = vec![0u8; len];
    let mut x = seed;
    for slot in out.chunks_mut(8) {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let b = x.to_le_bytes();
        for (s, v) in slot.iter_mut().zip(b.iter()) {
            *s = *v;
        }
    }
    *env.data().wasi_rng_seed.lock().unwrap() = x;
    view.write(ptr, &out).map_err(|_| wasmer::RuntimeError::new("random_get: OOB"))?;
    errno(0)
}

/// `proc_exit(code)` — the plugin asked to terminate; surface it as an error
/// on the in-flight call so the runner winds the instance down.
fn h_proc_exit(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    let code = args[0].unwrap_i32() as u32;
    *env.data().sbx.exited_code.lock().unwrap() = Some(code);
    Err(wasmer::RuntimeError::new(format!("wasm proc_exit({code})")))
}

fn h_proc_raise(env: FunctionEnvMut<HsEnv>, args: &[Value]) -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    let sig = args[0].unwrap_i32() as u32;
    *env.data().sbx.exited_code.lock().unwrap() = Some(sig);
    Err(wasmer::RuntimeError::new(format!("wasm proc_raise({sig}) — aborted")))
}

fn enosys() -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    errno(52) // ENOSYS
}

fn enoent() -> std::result::Result<Vec<Value>, wasmer::RuntimeError> {
    errno(44) // ENOENT — no file: the sandbox has no pre-opened host paths,
    // so file-stat / prestat / open all answer "not found". Callers that
    // probe the system font directory take the fallback path and use their
    // bundled face.
}

/// The whole `wasi_snapshot_preview1` namespace for one instance.
pub fn wasi_namespace(store: &mut wasmer::Store, env: &FunctionEnv<HsEnv>) -> wasmer::Exports {
    wasmer::import_namespace! {
        {
            "args_get" => Function::new_with_env(store, env, ty(3), h_args_get),
            "args_sizes_get" => Function::new_with_env(store, env, ty(2), h_args_sizes_get),
            "environ_get" => Function::new_with_env(store, env, ty(2), h_environ_get),
            "environ_sizes_get" => Function::new_with_env(store, env, ty(2), h_environ_sizes_get),
            "clock_res_get" => Function::new_with_env(store, env, ty(2), h_clock_res_get),
            "clock_time_get" => Function::new_with_env(
                store,
                env,
                FunctionType::new(vec![Type::I32, Type::I64, Type::I32], vec![Type::I32]),
                h_clock_time_get,
            ),
            "fd_close" => Function::new_with_env(store, env, ty(1), h_fd_close),
            "fd_fdstat_get" => Function::new_with_env(store, env, ty(2), h_fd_fdstat_get),
            "fd_read" => Function::new_with_env(store, env, ty(4), |_, _| enosys()),
            // No host files are pre-opened: stat/prestat/open all ENOENT.
            "fd_filestat_get" => Function::new_with_env(
                store,
                env,
                ty(2),
                |_, _| enoent(),
            ),
            "fd_prestat_get" => Function::new_with_env(store, env, ty(2), |_, _| enoent()),
            "fd_prestat_dir_name" => Function::new_with_env(
                store,
                env,
                ty(3),
                |_, _| enoent(),
            ),
            "fd_seek" => Function::new_with_env(
                store,
                env,
                FunctionType::new(vec![Type::I32, Type::I64, Type::I32, Type::I32], vec![Type::I32]),
                |_, _| enosys(),
            ),
            // `path_open(13 params)` — open relative to a pre-opened dir; the
            // sandbox pre-opens nothing, so every path is "not found".
            "path_open" => Function::new_with_env(
                store,
                env,
                FunctionType::new(
                    vec![
                        Type::I32,
                        Type::I32,
                        Type::I32,
                        Type::I32,
                        Type::I32,
                        Type::I64,
                        Type::I64,
                        Type::I32,
                        Type::I32,
                    ],
                    vec![Type::I32],
                ),
                |_, _| enoent(),
            ),
            // `path_filestat_get(fd, fsflags, pathlen, path, out)` — 5×i32,
            // pulled in by `fastrand` (via `phf`) when the plugin is built.
            // We never actually resolve a path — the sandbox pre-opens nothing —
            // so ENOENT is the correct, benign response.
            "path_filestat_get" => Function::new_with_env(
                store,
                env,
                FunctionType::new(
                    vec![Type::I32, Type::I32, Type::I32, Type::I32, Type::I32],
                    vec![Type::I32],
                ),
                |_, _| enoent(),
            ),
            "fd_write" => Function::new_with_env(store, env, ty(4), h_fd_write),
            "fd_writev" => Function::new_with_env(store, env, ty(4), h_fd_write),
            "proc_exit" => Function::new_with_env(
                store,
                env,
                FunctionType::new(vec![Type::I32], Vec::<Type>::new()),
                h_proc_exit,
            ),
            "proc_raise" => Function::new_with_env(
                store,
                env,
                FunctionType::new(vec![Type::I32], Vec::<Type>::new()),
                h_proc_raise,
            ),
            "random_get" => Function::new_with_env(store, env, ty(2), h_random_get),
            "sched_yield" => Function::new_with_env(store, env, ty(0), h_sched_yield),
        }
    }
}
