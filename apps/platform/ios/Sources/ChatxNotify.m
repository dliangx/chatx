/*
 * Implementation of ChatxNotify (see ChatxNotify.h).
 *
 * Posts a local notification via UNUserNotificationCenter. The first call
 * from a given process triggers a system authorization prompt (one-shot —
 * iOS shows it at most once per app). Subsequent posts after the user
 * has granted permission work normally; before the user has granted
 * (or if they denied), the post is queued by the system and delivered
 * once the user grants permission in Settings.
 */

#import "ChatxNotify.h"
#import <dispatch/dispatch.h>

// Monotonic counter for the UNNotificationRequest identifier. iOS allows
// duplicate identifiers to replace earlier notifications, but a unique id
// per call is safer for the "one per inbound message" semantic we want.
static long long s_nextReqId = 1000;

#pragma mark - Singleton

@interface ChatxNotify ()
@end

@implementation ChatxNotify

+ (instancetype)shared {
  static ChatxNotify *s_shared = nil;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    s_shared = [[self alloc] init];
  });
  return s_shared;
}

- (void)show:(NSString *)title body:(NSString *)body {
  if (title.length == 0 && body.length == 0)
    return;

  UNUserNotificationCenter *center = [UNUserNotificationCenter currentNotificationCenter];

  // Ask for authorization once. The completion handler fires after the
  // user's choice; we proceed with the post regardless (the system queues
  // it if not granted, so a later grant still surfaces it).
  [center requestAuthorizationWithOptions:(UNAuthorizationOptionAlert
                                            | UNAuthorizationOptionSound)
                           completionHandler:^(BOOL granted, NSError *err) {
    if (err) {
      NSLog(@"[chatx.notify] requestAuthorization error: %@", err);
    } else {
      NSLog(@"[chatx.notify] authorization granted=%d", granted);
    }

    // Post on the main thread — UNUserNotificationCenter's post method is
    // safe off-main, but the surrounding UI work (e.g. badge updates if
    // we ever add them) is cleaner here.
    dispatch_async(dispatch_get_main_queue(), ^{
      UNMutableNotificationContent *content = [UNMutableNotificationContent new];
      content.title = title ?: @"";
      content.body = body ?: @"";
      content.sound = [UNNotificationSound defaultSound];

      UNNotificationRequest *req =
          [UNNotificationRequest
              requestWithIdentifier:[NSString stringWithFormat:@"chatx.%lld",
                                    s_nextReqId++]
              content:content
              trigger:nil]; // nil trigger = deliver immediately

      [[UNUserNotificationCenter currentNotificationCenter]
          addNotificationRequest:req
          withCompletionHandler:^(NSError *postErr) {
            if (postErr) {
              NSLog(@"[chatx.notify] addNotificationRequest error: %@", postErr);
            }
          }];
    });
  }];
}

@end

#pragma mark - C-ABI entry point (called from Rust via shim_impl.rs)

void chatx_notify_show(const char *title, const char *body) {
  if (title == NULL && body == NULL)
    return;
  NSString *t = title ? [NSString stringWithUTF8String:title] : @"";
  NSString *b = body ? [NSString stringWithUTF8String:body] : @"";
  [[ChatxNotify shared] show:t body:b];
}
