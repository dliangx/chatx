/*
 * Screen capture for Chatx iOS.
 *
 * Wraps RPScreenRecorder (in-app screen share, deprecated in iOS 27 but
 * still shipped and functional — see `apps/platform/ios/Sources/
 * ChatxScreenCapture.h` for the C-ABI surface, and
 * `crates/bridge/src/shim_impl.rs` for the extern "C" side).
 *
 * The recorder's `captureHandler` receives `CMSampleBuffer`s with
 * `CVPixelBuffer`s documented as `kCVPixelFormatType_32BGRA` from
 * RPScreenRecorder's public spec. We flatten each frame into a packed `w*h*4`
 * BGRA buffer and push it through `bridge_screen_frame_in` (from
 * `crates/bridge/src/ffi.rs`) with `fmt = 7` (PixelFormat::Bgra8888).
 *
 * Threading:
 *   - The RPScreenRecorder APIs require the main thread.
 *   - The `captureHandler` runs on RPScreenRecorder's private dispatch queue.
 *     It just does the pixel copy + FFI call. The Rust sink callback
 *     (`handle_screen_frame` in apps/chat) decides what to do next.
 */

#import "ChatxScreenCapture.h"
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ReplayKit/ReplayKit.h>
#import <dispatch/dispatch.h>

// From crates/bridge/src/ffi.rs. `bridge_screen_frame_in` is declared
// extern "C" with `#[unsafe(no_mangle)]`, so the symbol is plain.
extern int bridge_screen_frame_in(const void *data, unsigned long len,
                                  unsigned int width, unsigned int height,
                                  int fmt);

// Bridge PixelFormat::Bgra8888 (see crates/bridge/src/types.rs).
#define CHATX_FMT_BGRA8888 7

#pragma mark - Singleton

@interface ChatxScreenCapture ()
@property(nonatomic, assign) BOOL isRunning;
@end

@implementation ChatxScreenCapture {
  BOOL _isRunning;
}

+ (instancetype)shared {
  static ChatxScreenCapture *s_shared = nil;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    s_shared = [[self alloc] init];
  });
  return s_shared;
}

- (BOOL)isRunning {
  return _isRunning;
}
- (void)setIsRunning:(BOOL)v {
  _isRunning = v;
}

- (void)start {
  if (_isRunning)
    return;
  if (![RPScreenRecorder sharedRecorder].isAvailable) {
    NSLog(@"[chatx] RPScreenRecorder not available");
    return;
  }
  _isRunning = YES;
  RPScreenRecorder *rec = [RPScreenRecorder sharedRecorder];
  rec.microphoneEnabled = YES;

  // Capture loop — runs on RPScreenRecorder's internal queue.
  void (^captureHandler)(CMSampleBufferRef sbuf, RPSampleBufferType type,
                         NSError *e) =
      ^(CMSampleBufferRef sbuf, RPSampleBufferType type, NSError *err) {
        if (!sbuf)
          return;
        if (type != RPSampleBufferTypeVideo)
          return;
        CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sbuf);
        if (!pb)
          return;

        size_t w = CVPixelBufferGetWidth(pb);
        size_t h = CVPixelBufferGetHeight(pb);
        size_t rowBytes = CVPixelBufferGetBytesPerRow(pb);
        CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
        const uint8_t *src = (const uint8_t *)CVPixelBufferGetBaseAddress(pb);
        if (!src || rowBytes == 0) {
          CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
          return;
        }
        size_t packed = w * h * 4;
        uint8_t *out = (uint8_t *)malloc(packed);
        if (out == NULL) {
          CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
          return;
        }
        for (size_t y = 0; y < h; y++) {
          memcpy(out + y * w * 4, src + y * rowBytes, w * 4);
        }
        CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);

        (void)bridge_screen_frame_in(out, packed, (unsigned int)w,
                                     (unsigned int)h, CHATX_FMT_BGRA8888);
        free(out);
      };

  dispatch_async(dispatch_get_main_queue(), ^{
    [rec startCaptureWithHandler:captureHandler
               completionHandler:^(NSError *err) {
                 (void)err;
                 dispatch_async(dispatch_get_main_queue(), ^{
                   [ChatxScreenCapture shared].isRunning = NO;
                   NSLog(@"[chatx] RPScreenRecorder capture ended (err=%@)",
                         err);
                 });
               }];
  });
}

- (void)stop {
  if (!_isRunning)
    return;
  _isRunning = NO;
  dispatch_async(dispatch_get_main_queue(), ^{
    [[RPScreenRecorder sharedRecorder] stopCaptureWithHandler:^(NSError *err) {
      if (err)
        NSLog(@"[chatx] stopCapture: %@", err);
      else
        NSLog(@"[chatx] screen capture stopped");
    }];
  });
}

- (BOOL)isAvailable {
  return [RPScreenRecorder sharedRecorder].isAvailable;
}
@end

#pragma mark - C-ABI entry points (called from Rust via shims.rs)

void chatx_screen_capture_start(void) { [[ChatxScreenCapture shared] start]; }

void chatx_screen_capture_stop(void) { [[ChatxScreenCapture shared] stop]; }

int chatx_screen_capture_isAvailable(void) {
  return [[ChatxScreenCapture shared] isAvailable] ? 1 : 0;
}

int chatx_screen_capture_isActive(void) {
  return [ChatxScreenCapture shared].isRunning ? 1 : 0;
}
