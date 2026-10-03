/*
 * Screen capture for Chatx iOS (C-ABI surface + ObjC class).
 *
 * Wraps RPScreenRecorder (in-app screen share, deprecated in iOS 27 but
 * still shipped and functional). The class delivers `CVPixelBuffer`s; we
 * copy each frame into a packed `w*h*4` BGRA buffer and push it through
 * the C-ABI `bridge_screen_frame_in` from `crates/bridge/src/ffi.rs`.
 *
 * C-ABI surface (linked into libchatx):
 *   void chatx_screen_capture_start(void);
 *   void chatx_screen_capture_stop(void);
 *   int  chatx_screen_capture_isAvailable(void);
 *   int  chatx_screen_capture_isActive(void);
 *
 * ObjC class (used by the C-ABI entry points internally):
 *   ChatxScreenCapture
 *     + shared
 *     - start / stop / isAvailable
 */

#import <Foundation/Foundation.h>
#import <ReplayKit/ReplayKit.h>
#import <CoreVideo/CoreVideo.h>
#import <CoreMedia/CoreMedia.h>

#ifdef __cplusplus
extern "C" {
#endif

void chatx_screen_capture_start(void);
void chatx_screen_capture_stop(void);
int  chatx_screen_capture_isAvailable(void);
int  chatx_screen_capture_isActive(void);

#ifdef __cplusplus
}
#endif

@interface ChatxScreenCapture : NSObject
+ (instancetype)shared;
- (void)start;
- (void)stop;
- (BOOL)isAvailable;
@property (nonatomic, readonly) BOOL isRunning;
@end
