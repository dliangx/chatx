//! macOS camera capture + native QR-code recognition via AVFoundation.
#![allow(unsafe_op_in_unsafe_fn, non_snake_case, deprecated)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, AllocAnyThread};
use objc2_av_foundation::{
    AVMediaTypeVideo, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput,
    AVCaptureMetadataOutput, AVCaptureMetadataOutputObjectsDelegate, AVCaptureSession,
    AVAuthorizationStatus, AVMetadataMachineReadableCodeObject,
};
use dispatch2::{DispatchQueue, DispatchQoS, GlobalQueueIdentifier};

/// Global sink, installed via [`Camera::set_sink`] and called from the
/// AVFoundation metadata callback queue with every decoded QR payload.
/// Owner must be `Send`.
static SINK: std::sync::OnceLock<std::sync::Mutex<Box<dyn Fn(String) + Send>>> =
    std::sync::OnceLock::new();

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

/// A (possibly running) AVFoundation capture session with QR recognition.
pub struct Camera {
    session: Option<Retained<AVCaptureSession>>,
    delegate: Option<Retained<QrDelegate>>,
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

    /// Open the default camera and start the metadata (QR) output pipeline.
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
        let delegate: Retained<QrDelegate> = unsafe { objc2::msg_send![alloc, init] };
        unsafe {
            let queue = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
                DispatchQoS::Default,
            ));
            let proto: &ProtocolObject<dyn AVCaptureMetadataOutputObjectsDelegate> =
                ProtocolObject::from_ref(&*delegate);
            output.setMetadataObjectsDelegate_queue(Some(proto), Some(&*queue));
        }
        self.delegate = Some(delegate);

        unsafe { session.startRunning() };

        self.session = Some(session);
        self.running.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stop the capture session; keeps the instance ready for a later start.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(s) = self.session.take() {
            unsafe { s.stopRunning() };
        }
        self.delegate = None;
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop();
    }
}
