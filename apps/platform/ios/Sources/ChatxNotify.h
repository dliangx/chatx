/*
 * System notification for Chatx iOS.
 *
 * Wraps UNUserNotificationCenter (UserNotifications.framework) to post
 * a local notification with a title + body. The Rust core calls
 * `chatx_notify_show(title, body)` (a plain C-ABI function with
 * NUL-terminated UTF-8 `char*` arguments) from
 * `crates/bridge/src/shim_impl.rs`.
 *
 * C-ABI surface (linked into libchatx):
 *   void chatx_notify_show(const char *title, const char *body);
 *
 * ObjC class (used internally by the C-ABI entry point):
 *   ChatxNotify
 *     + shared
 *     - show:(NSString*)title body:(NSString*)body
 *
 * Threading: UNUserNotificationCenter APIs are safe to call from any
 * thread, but the authorization prompt is best fired on the main thread
 * (it is the system's UI). We hop to main for the prompt + post.
 *
 * Note: `UNUserNotificationCenter` does NOT require an App Bundle to
 * post notifications — it works from a plain CLI / cdylib executable
 * (the system falls back to a basic prompt). This is the path the
 * existing RPScreenRecorder shim already takes.
 */

#import <Foundation/Foundation.h>
#import <UserNotifications/UserNotifications.h>

#ifdef __cplusplus
extern "C" {
#endif

void chatx_notify_show(const char *title, const char *body);

#ifdef __cplusplus
}
#endif

@interface ChatxNotify : NSObject
+ (instancetype)shared;
- (void)show:(NSString *)title body:(NSString *)body;
@end
