//! macOS camera capture + native QR-code recognition via AVFoundation.
//! On top of the QR pipeline we also expose still-image capture via
//! `AVCapturePhotoOutput` (the non-deprecated photo API) — `Camera::capture`
//! triggers a photo which is delivered to the sink installed via
//! `Camera::set_capture_sink` as a JPEG file path once it has been saved.
#![allow(unsafe_op_in_unsafe_fn, non_snake_case, deprecated)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_av_foundation::{
    AVMediaTypeVideo, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureMetadataOutput, AVCaptureMetadataOutputObjectsDelegate, AVCapturePhoto,
    AVCapturePhotoCaptureDelegate, AVCapturePhotoOutput, AVCapturePhotoSettings,
    AVCaptureSession, AVAuthorizationStatus, AVMetadataMachineReadableCodeObject,
};
use objc2_foundation::{NSError, NSString};
use dispatch2::{DispatchQueue, DispatchQoS, GlobalQueueIdentifier};

/// Global sink, installed via [`Camera::set_sink`] and called from the
/// AVFoundation metadata callback queue with every decoded QR payload.
/// Owner must be `Send`.
static SINK: std::sync::OnceLock<std::sync::Mutex<Box<dyn Fn(String) + Send>>> =
    std::sync::OnceLock::new();

/// Global sink, installed via [`Camera::set_capture_sink`] and called with the
/// saved photo path (or an error) once a `Camera::capture` finishes.
type CaptureResult = Result<String, String>;
static CAPTURE_SINK: OnceLock<Mutex<Box<dyn Fn(CaptureResult) + Send>>> = OnceLock::new();

/// Destination for the next captured photo. Written by `Camera::capture`
/// right before triggering, read by `PhotoDelegate::captureOutput_didFinishProcessingPhoto_error`
/// when the ObjC callback fires.
static LAST_CAPTURE_PATH: Mutex<std::path::PathBuf> = Mutex::new(std::path::PathBuf::new());

define_class!(
    /// ObjC delegate forwarding metadata objects to the Rust sink.
    #[unsafe(super = NSObject)]
    struct QrDelegate;

    unsafe impl NSObjectProtocol for QrDelegate {}

    unsafe impl AVCaptureMetadataOutputObjectsDelegate for QrDelegate {
        #[unsafe(method(captureOutput:didOutputMetadataObjects:fromConnection:))]
        unsafe fn captureOutput_didOutputMetadataObjects_fromConnection(
            &self,
            _output: &AVCaptureMetadataOutput,
            objects: &objc2_foundation::NSArray<objc2_av_foundation::AVMetadataObject>,
            _connection: &AVCaptureConnection,
        ) {
            for i in 0usize..objects.count() {
                let obj = objects.objectAtIndex(i);
                let Ok(code) = obj.downcast::<AVMetadataMachineReadableCodeObject>() else {
                    continue;
                };
                let Some(s) = code.stringValue() else {
                    continue;
                };
                let payload: String = s.to_string();
                if !payload.trim().is_empty() {
                    if let Some(slot) = SINK.get() {
                        if let Ok(g) = slot.lock() {
                            (g)(payload);
                        }
                    }
                }
            }
        }
    }
);

define_class!(
    /// ObjC delegate that saves the processed `AVCapturePhoto` to `LAST_CAPTURE_PATH`
    /// and forwards the resulting path (or error) to `CAPTURE_SINK`.
    #[unsafe(super = NSObject)]
    struct PhotoDelegate;

    unsafe impl NSObjectProtocol for PhotoDelegate {}

    unsafe impl AVCapturePhotoCaptureDelegate for PhotoDelegate {
        #[unsafe(method(captureOutput:didFinishProcessingPhoto:error:))]
        unsafe fn captureOutput_didFinishProcessingPhoto_error(
            &self,
            _output: &AVCapturePhotoOutput,
            photo: &AVCapturePhoto,
            error: Option<&NSError>,
        ) {
            if let Some(err) = error {
                let desc: String = err.localizedDescription().to_string();
                notify_capture(Err(desc));
                return;
            }
            let data = photo.fileDataRepresentation();
            let Some(data) = data else {
                notify_capture(Err("照片数据为空".to_string()));
                return;
            };
            let path = LAST_CAPTURE_PATH
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            if path.as_os_str().is_empty()
                || data.writeToFile_atomically(&NSString::from_str(path.to_str().unwrap_or("")), false)
            {
                let saved = path.to_string_lossy().to_string();
                notify_capture(Ok(saved));
            } else {
                notify_capture(Err("保存图片失败".to_string()));
            }
        }
    }
);

fn notify_capture(res: CaptureResult) {
    if let Some(slot) = CAPTURE_SINK.get() {
        if let Ok(g) = slot.lock() {
            (g)(res);
        }
    }
}

/// A (possibly running) AVFoundation capture session with QR recognition + still photo.
pub struct Camera {
    session: Option<Retained<AVCaptureSession>>,
    delegate: Option<Retained<QrDelegate>>,
    photo_output: Option<Retained<AVCapturePhotoOutput>>,
    photo_delegate: Option<Retained<PhotoDelegate>>,
    running: Arc<AtomicBool>,
}

impl Default for Camera {
    fn default() -> Self {
        Self::new()
    }
}

impl Camera {
    pub fn new() -> Self {
        Camera {
            session: None,
            delegate: None,
            photo_output: None,
            photo_delegate: None,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Install the sink used for decoded QR payloads. Can be called any time;
    /// the next decode event uses the newest closure.
    pub fn set_sink<F: Fn(String) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink);
            }
        } else {
            let _ = SINK.set(std::sync::Mutex::new(Box::new(sink)));
        }
    }

    /// Install the sink used for photo-capture results. Called with the saved
    /// file path on success, or an error message on failure.
    pub fn set_capture_sink<F: Fn(CaptureResult) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = CAPTURE_SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink);
            }
        } else {
            let _ = CAPTURE_SINK.set(Mutex::new(Box::new(sink)));
        }
    }

    /// Open the default camera and start the metadata (QR) output + photo output.
    pub fn start(&mut self) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        // If the user has already denied camera access there is no point
        // continuing — surface a clear action. When the status is
        // "not determined" the TCC dialog is shown automatically by the
        // subsequent AVCaptureDeviceInput creation (see AVCaptureDevice docs).
        let video_type = unsafe { AVMediaTypeVideo }.expect("AVMediaTypeVideo");
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(video_type) };
        if status == AVAuthorizationStatus::Denied {
            return Err(
                "未授权摄像头访问，请前往 系统设置 › 隐私与安全性 › 摄像头 为本应用授权后重试"
                    .to_string(),
            );
        }

        let session = unsafe { AVCaptureSession::new() };
        let devices = unsafe { AVCaptureDevice::devices() };
        let Some(device) = devices.firstObject() else {
            return Err("未找到可用的摄像头设备".to_string());
        };
        let input: Retained<AVCaptureDeviceInput> =
            unsafe { AVCaptureDeviceInput::deviceInputWithDevice_error(&device) }
                .map_err(|e| format!("创建 AVCaptureDeviceInput 失败: {e}"))?;

        if !unsafe { session.canAddInput(&input) } {
            return Err("摄像头无法加入会话（请确认已授权访问权限）".to_string());
        }
        unsafe { session.addInput(&input) };

        let output = unsafe { AVCaptureMetadataOutput::new() };
        unsafe {
            let types = output.availableMetadataObjectTypes();
            output.setMetadataObjectTypes(Some(&types));
        }
        if !unsafe { session.canAddOutput(&output) } {
            return Err("添加二维码识别输出失败".to_string());
        }
        unsafe { session.addOutput(&output) };

        let alloc = QrDelegate::alloc();
        let delegate: Retained<QrDelegate> = unsafe { msg_send![alloc, init] };
        unsafe {
            let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
                DispatchQoS::Default,
            ));
            let proto: &ProtocolObject<dyn AVCaptureMetadataOutputObjectsDelegate> =
                ProtocolObject::from_ref(&*delegate);
            output.setMetadataObjectsDelegate_queue(Some(proto), Some(&*queue));
        }
        self.delegate = Some(delegate);

        // Photo output: attach so a later `capture()` can request a photo without
        // needing to reconfigure the session.
        let photo_out = unsafe { AVCapturePhotoOutput::new() };
        if unsafe { session.canAddOutput(&photo_out) } {
            unsafe { session.addOutput(&photo_out) };
            self.photo_output = Some(photo_out);
        }

        let palloc = PhotoDelegate::alloc();
        let pdelegate: Retained<PhotoDelegate> = unsafe { msg_send![palloc, init] };
        self.photo_delegate = Some(pdelegate);

        unsafe { session.startRunning() };

        self.session = Some(session);
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Trigger a photo capture. The saved file path is delivered asynchronously
    /// to the sink installed via [`Camera::set_capture_sink`].
    ///
    /// No `&mut` needed to be taken after the first `start()` — the photo
    /// output and delegate persist on the instance.
    pub fn capture(&mut self) -> Result<(), String> {
        if !self.running.load(Ordering::SeqCst) {
            return Err("摄像头未启动".to_string());
        }
        let Some(out) = &self.photo_output else {
            return Err("未找到照片输出".to_string());
        };
        let Some(dg) = &self.photo_delegate else {
            return Err("未找到照片回调".to_string());
        };

        // Unique destination path (temp dir, timestamp+random-ish suffix).
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("chatx_photo_{ts_ms}.jpg"));
        {
            let mut g = LAST_CAPTURE_PATH
                .lock()
                .map_err(|_| "capture path locked".to_string())?;
            g.clone_from(&path);
        }

        let settings = unsafe { AVCapturePhotoSettings::photoSettings() };
        let proto: &ProtocolObject<dyn AVCapturePhotoCaptureDelegate> =
            ProtocolObject::from_ref(&**dg);
        unsafe {
            out.capturePhotoWithSettings_delegate(&settings, proto);
        }
        Ok(())
    }

    /// Stop the capture session; keeps the instance ready for a later start.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(s) = self.session.take() {
            unsafe { s.stopRunning() };
        }
        self.delegate = None;
        self.photo_output = None;
        self.photo_delegate = None;
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop();
    }
}
