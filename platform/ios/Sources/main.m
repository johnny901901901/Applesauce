#include <SDL.h>
#include <SDL_system.h>
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <objc/message.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/sysctl.h>
#include <sys/types.h>
#include <unistd.h>

#import <Foundation/Foundation.h>
#import <AVFoundation/AVFoundation.h>
#import <GameController/GameController.h>

// Each emulator core exports this; the host passes in the one belonging to the
// core it loaded for this game.
typedef int32_t (*TouchHLEIOSRunGameFn)(
    const char *path,
    int32_t scale_hack,
    int32_t orientation,
    int32_t network_access,
    int32_t analog_stick_tilt_controls
);

// Dynarmic needs writable-executable memory, and two things can grant it:
//
//   * An attached debugger (StikDebug, AltJIT, TrollStore's "Enable JIT"),
//     which sets CS_DEBUGGED on the process. This has to be redone every time
//     the app starts as a new process.
//   * The `dynamic-codesigning` entitlement, which makes it permanent. Only
//     TrollStore can grant that, and iOS 15+ only honours it on A11 and older
//     chips.
//
// Checking only for a debugger would tell a TrollStore user with permanent JIT
// that they have none, and hold back a launch that would have worked.

// Not declared in the public SDK, but a stable syscall wrapper in libSystem.
extern int csops(pid_t pid, unsigned int ops, void *useraddr, size_t usersize);
#define TOUCHHLE_CS_OPS_STATUS 0
#define TOUCHHLE_CS_DEBUGGED 0x10000000

bool touchhle_ios_jit_is_from_debugger(void) {
    // dynarmic's code allocator asks for its executable memory by executing
    // `brk #0xf00d` (see oaknut's prepare_jit_region), a trap that only an
    // attached debugger can service. With none attached the trap is fatal the
    // instant a game starts, which reads as the app quitting for no reason.
    unsigned int flags = 0;
    if (csops(getpid(), TOUCHHLE_CS_OPS_STATUS, &flags, sizeof(flags)) == 0
        && (flags & TOUCHHLE_CS_DEBUGGED) != 0) {
        return true;
    }

    // P_TRACED is the older signal for the same thing, and is what this app
    // shipped with through 0.3.0. Keep it as a second opinion rather than
    // replace it.
    struct kinfo_proc info;
    info.kp_proc.p_flag = 0;

    int mib[4] = {CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()};
    size_t size = sizeof(info);
    if (sysctl(mib, 4, &info, &size, NULL, 0) != 0) {
        return false;
    }

    return (info.kp_proc.p_flag & P_TRACED) != 0;
}

// SecTask lives in Security.framework but is not declared in the iOS SDK.
// Resolve it at runtime so a missing symbol degrades to "no entitlement"
// rather than stopping the app from launching at all.
static bool touchhle_has_dynamic_codesigning(void) {
    typedef CFTypeRef (*create_from_self_fn)(CFAllocatorRef);
    typedef CFTypeRef (*copy_entitlement_fn)(CFTypeRef, CFStringRef, CFErrorRef *);

    static create_from_self_fn create_task;
    static copy_entitlement_fn copy_entitlement;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        void *security = dlopen(
            "/System/Library/Frameworks/Security.framework/Security",
            RTLD_LAZY
        );
        if (security == NULL) {
            return;
        }
        create_task = (create_from_self_fn)dlsym(security, "SecTaskCreateFromSelf");
        copy_entitlement =
            (copy_entitlement_fn)dlsym(security, "SecTaskCopyValueForEntitlement");
    });

    if (create_task == NULL || copy_entitlement == NULL) {
        return false;
    }

    CFTypeRef task = create_task(kCFAllocatorDefault);
    if (task == NULL) {
        return false;
    }
    CFTypeRef value = copy_entitlement(task, CFSTR("dynamic-codesigning"), NULL);
    bool granted = value != NULL
        && CFGetTypeID(value) == CFBooleanGetTypeID()
        && CFBooleanGetValue((CFBooleanRef)value);
    if (value != NULL) {
        CFRelease(value);
    }
    CFRelease(task);
    return granted;
}

bool touchhle_ios_jit_available(void) {
    return touchhle_ios_jit_is_from_debugger() || touchhle_has_dynamic_codesigning();
}

// Do NOT add an mmap PROT_WRITE | PROT_EXEC probe here. Per mmap(2), iOS
// returns a writable-but-not-executable mapping instead of failing when
// MAP_JIT is absent, so that probe reports success with JIT off. A false "JIT
// is on" sends the emulator into a guaranteed freeze with no explanation.
void touchhle_ios_log_jit_status(const char *context) {
    unsigned int flags = 0;
    int result = csops(getpid(), TOUCHHLE_CS_OPS_STATUS, &flags, sizeof(flags));
    fprintf(
        stderr,
        "touchHLE JIT [%s]: available=%d debugger=%d entitlement=%d "
        "cs_flags=0x%08x csops=%d/%d\n",
        context ? context : "?",
        touchhle_ios_jit_available(),
        touchhle_ios_jit_is_from_debugger(),
        touchhle_has_dynamic_codesigning(),
        flags,
        result,
        (result == 0) ? 0 : errno
    );
}

// What iOS itself reports, independent of SDL: tells a controller the system
// never gives the app apart from one SDL fails to pick up.
static void touchhle_ios_log_controllers(const char *context) {
    NSArray<GCController *> *controllers = [GCController controllers];
    fprintf(stderr, "touchHLE controllers [%s]: %lu\n", context,
            (unsigned long)controllers.count);
    for (GCController *controller in controllers) {
        fprintf(stderr, "- %s (%s) extended=%d\n",
                controller.vendorName.UTF8String ?: "?",
                controller.productCategory.UTF8String ?: "?",
                controller.extendedGamepad != nil);
    }
}

static void touchhle_ios_observe_controllers(void) {
    NSNotificationCenter *center = [NSNotificationCenter defaultCenter];
    [center addObserverForName:GCControllerDidConnectNotification
                        object:nil
                         queue:nil
                    usingBlock:^(NSNotification *note) {
        GCController *controller = note.object;
        fprintf(stderr, "touchHLE controller connected: %s\n",
                controller.vendorName.UTF8String ?: "?");
    }];
    [center addObserverForName:GCControllerDidDisconnectNotification
                        object:nil
                         queue:nil
                    usingBlock:^(NSNotification *note) {
        GCController *controller = note.object;
        fprintf(stderr, "touchHLE controller disconnected: %s\n",
                controller.vendorName.UTF8String ?: "?");
    }];
}

// Without a configured session iOS uses SoloAmbient, which is silenced by the
// Ring/Silent switch; games are expected to be heard regardless.
static void touchhle_ios_configure_audio(void) {
    AVAudioSession *session = [AVAudioSession sharedInstance];
    NSError *error = nil;
    if (![session setCategory:AVAudioSessionCategoryPlayback error:&error]) {
        fprintf(stderr, "touchHLE audio: setCategory failed: %s\n",
                error.localizedDescription.UTF8String ?: "?");
    }
    error = nil;
    if (![session setActive:YES error:&error]) {
        fprintf(stderr, "touchHLE audio: setActive failed: %s\n",
                error.localizedDescription.UTF8String ?: "?");
    }
}

static FILE *diagnostic_log;

static void redirect_diagnostics(void) {
    const char *home = getenv("HOME");
    if (home == NULL) {
        return;
    }

    char log_path[PATH_MAX];
    int length = snprintf(log_path, sizeof(log_path), "%s/Documents/touchhle-host.log", home);
    if (length < 0 || (size_t)length >= sizeof(log_path)) {
        return;
    }

    // If a game hangs, the only way out is to force-quit, and the next launch
    // would truncate the log that recorded the hang. Keep one generation back
    // so the interesting session survives the relaunch needed to retrieve it.
    char previous_path[PATH_MAX];
    length = snprintf(
        previous_path,
        sizeof(previous_path),
        "%s/Documents/touchhle-host-previous.log",
        home
    );
    if (length > 0 && (size_t)length < sizeof(previous_path)) {
        rename(log_path, previous_path);
    }

    diagnostic_log = fopen(log_path, "w");
    if (diagnostic_log == NULL) {
        return;
    }

    setvbuf(diagnostic_log, NULL, _IONBF, 0);
    dup2(fileno(diagnostic_log), STDOUT_FILENO);
    dup2(fileno(diagnostic_log), STDERR_FILENO);
    fprintf(stderr, "touchHLE iOS port diagnostics started\n");
}

static void start_native_host(void) {
    Class host_class = NSClassFromString(@"TouchHLENativeHost");
    SEL selector = NSSelectorFromString(@"start");
    if (host_class == Nil || ![host_class respondsToSelector:selector]) {
        fprintf(stderr, "Could not start the native iOS port UI\n");
        return;
    }

    ((void (*)(id, SEL))objc_msgSend)(host_class, selector);
}

int32_t touchhle_ios_launch_game(
    TouchHLEIOSRunGameFn run_game,
    const char *path,
    int32_t scale_hack,
    int32_t orientation,
    int32_t network_access,
    int32_t analog_stick_tilt_controls
) {
    if (run_game == NULL) {
        fprintf(stderr, "touchHLE failed: no emulator core was loaded\n");
        return 1;
    }

    const char *orientation_hint = "Portrait";
    if (orientation == 1) {
        orientation_hint = "LandscapeLeft";
    } else if (orientation == 2) {
        orientation_hint = "LandscapeRight";
    }
    SDL_SetHint(SDL_HINT_ORIENTATIONS, orientation_hint);

    // Breadcrumbs: the emulator runs on the main thread, so if it hangs the UI
    // freezes with it and the log is the only way to see how far it got.
    touchhle_ios_log_jit_status("game-launch");
    touchhle_ios_log_controllers("game-launch");
    fprintf(
        stderr,
        "touchHLE: entering emulator: scale_hack=%d orientation=%s network=%d "
        "analog_tilt=%d\n",
        scale_hack,
        orientation_hint,
        network_access,
        analog_stick_tilt_controls
    );

    SDL_iPhoneSetEventPump(SDL_TRUE);
    int32_t result = run_game(
        path,
        scale_hack,
        orientation,
        network_access,
        analog_stick_tilt_controls
    );
    SDL_iPhoneSetEventPump(SDL_FALSE);
    SDL_ResetHint(SDL_HINT_ORIENTATIONS);

    fprintf(stderr, "touchHLE: emulator returned %d\n", result);
    return result;
}

int main(int argc, char *argv[]) {
    (void)argc;
    (void)argv;

    redirect_diagnostics();
    touchhle_ios_log_jit_status("launch");
    touchhle_ios_observe_controllers();
    touchhle_ios_configure_audio();
    touchhle_ios_log_controllers("launch");

    char *base_path = SDL_GetBasePath();
    if (base_path != NULL) {
        chdir(base_path);
        SDL_free(base_path);
    }

    start_native_host();
    return 0;
}
