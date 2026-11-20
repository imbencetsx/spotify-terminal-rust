#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#import <MediaPlayer/MediaPlayer.h>
#include <libproc.h>
#include <unistd.h>

static AVAudioEngine *engine;
static AVAudioPlayerNode *node;
static NSMutableDictionary *metadata;
static NSString *coverURL;
static NSURLSessionDataTask *coverTask;
static MPMediaItemArtwork *artwork;
static NSDictionary *snapshot;

static MPRemoteCommandHandlerStatus emit(NSString *action, id value) {
    NSDictionary *event = value ? @{@"action": action, @"value": value} : @{@"action": action};
    NSData *json = [NSJSONSerialization dataWithJSONObject:event options:0 error:nil];
    if (!json) return MPRemoteCommandHandlerStatusCommandFailed;
    flockfile(stdout);
    fwrite(json.bytes, 1, json.length, stdout);
    fputc('\n', stdout);
    fflush(stdout);
    funlockfile(stdout);
    return MPRemoteCommandHandlerStatusSuccess;
}

static void bindAction(MPRemoteCommand *command, NSString *action) {
    command.enabled = YES;
    [command addTargetWithHandler:^MPRemoteCommandHandlerStatus(MPRemoteCommandEvent *event) {
        return emit(action, nil);
    }];
}

static void registerCommands(void) {
    MPRemoteCommandCenter *center = [MPRemoteCommandCenter sharedCommandCenter];
    bindAction(center.playCommand, @"play");
    bindAction(center.pauseCommand, @"pause");
    bindAction(center.togglePlayPauseCommand, @"toggle");
    bindAction(center.nextTrackCommand, @"next");
    bindAction(center.previousTrackCommand, @"previous");
    center.changePlaybackPositionCommand.enabled = YES;
    [center.changePlaybackPositionCommand addTargetWithHandler:^MPRemoteCommandHandlerStatus(MPRemoteCommandEvent *event) {
        return emit(@"seek", @(((MPChangePlaybackPositionCommandEvent *)event).positionTime));
    }];
    center.changeShuffleModeCommand.enabled = YES;
    [center.changeShuffleModeCommand addTargetWithHandler:^MPRemoteCommandHandlerStatus(MPRemoteCommandEvent *event) {
        return emit(@"shuffle", @(((MPChangeShuffleModeCommandEvent *)event).shuffleType != MPShuffleTypeOff));
    }];
    center.changeRepeatModeCommand.enabled = YES;
    [center.changeRepeatModeCommand addTargetWithHandler:^MPRemoteCommandHandlerStatus(MPRemoteCommandEvent *event) {
        MPRepeatType mode = ((MPChangeRepeatModeCommandEvent *)event).repeatType;
        return emit(@"repeat", mode == MPRepeatTypeOne ? @"track" : mode == MPRepeatTypeAll ? @"context" : @"off");
    }];
}

// Soloist owns the audible stream. A silent local stream lets macOS route the
// system controls to this bundled helper while str is actively playing.
static void startAudio(void) {
    if (engine.isRunning && node.isPlaying) return;
    engine = [[AVAudioEngine alloc] init];
    node = [[AVAudioPlayerNode alloc] init];
    AVAudioFormat *format = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:44100 channels:1];
    AVAudioPCMBuffer *buffer = [[AVAudioPCMBuffer alloc] initWithPCMFormat:format frameCapacity:4410];
    buffer.frameLength = buffer.frameCapacity;
    memset(buffer.floatChannelData[0], 0, buffer.frameLength * sizeof(float));
    [engine attachNode:node];
    [engine connect:node to:engine.mainMixerNode format:format];
    NSError *error = nil;
    if ([engine startAndReturnError:&error]) {
        [node scheduleBuffer:buffer atTime:nil options:AVAudioPlayerNodeBufferLoops completionHandler:nil];
        [node play];
    } else {
        NSLog(@"str Now Playing could not open its audio session: %@", error.localizedDescription);
    }
}

static void publish(void) {
    double rate = [snapshot[@"rate"] doubleValue];
    double seconds = [snapshot[@"position"] doubleValue];
    if (rate > 0) {
        double timestamp = [snapshot[@"timestamp"] doubleValue] / 1000;
        seconds += rate * MAX(0, NSDate.date.timeIntervalSince1970 - timestamp);
    }
    double duration = [snapshot[@"duration"] doubleValue];
    metadata[MPNowPlayingInfoPropertyElapsedPlaybackTime] = @(duration > 0 ? MIN(seconds, duration) : seconds);
    [MPNowPlayingInfoCenter defaultCenter].nowPlayingInfo = metadata;
}

static void updateCover(NSString *url) {
    if ((!url && !coverURL) || [url isEqualToString:coverURL]) return;
    [coverTask cancel];
    coverURL = [url copy];
    artwork = nil;
    NSURL *source = url ? [NSURL URLWithString:url] : nil;
    if (!source || ![@[@"http", @"https"] containsObject:source.scheme.lowercaseString]) return;
    coverTask = [[NSURLSession sharedSession] dataTaskWithURL:source
        completionHandler:^(NSData *data, NSURLResponse *response, NSError *error) {
        if (error || !data) return;
        dispatch_async(dispatch_get_main_queue(), ^{
            if (![url isEqualToString:coverURL] || !metadata) return;
            NSImage *image = [[NSImage alloc] initWithData:data];
            if (!image || image.size.width <= 0 || image.size.height <= 0) return;
            artwork = [[MPMediaItemArtwork alloc] initWithBoundsSize:image.size
                requestHandler:^NSImage *(CGSize size) { return image; }];
            metadata[MPMediaItemPropertyArtwork] = artwork;
            publish();
        });
    }];
    [coverTask resume];
}

static void clear(void) {
    updateCover(nil);
    metadata = nil;
    snapshot = nil;
    MPNowPlayingInfoCenter *center = [MPNowPlayingInfoCenter defaultCenter];
    center.nowPlayingInfo = nil;
    center.playbackState = MPNowPlayingPlaybackStateStopped;
    [node stop];
    [engine stop];
}

static void update(NSDictionary *state) {
    if (![state[@"title"] isKindOfClass:NSString.class]) {
        clear();
        return;
    }
    BOOL playing = [state[@"playing"] boolValue];
    snapshot = state;
    if (playing) startAudio();
    updateCover([state[@"cover"] isKindOfClass:NSString.class] ? state[@"cover"] : nil);
    metadata = [@{
        MPMediaItemPropertyTitle: state[@"title"],
        MPMediaItemPropertyArtist: state[@"artist"],
        MPMediaItemPropertyAlbumTitle: state[@"album"],
        MPMediaItemPropertyPlaybackDuration: state[@"duration"],
        MPNowPlayingInfoPropertyPlaybackRate: state[@"rate"],
        MPNowPlayingInfoPropertyMediaType: @(MPNowPlayingInfoMediaTypeAudio),
    } mutableCopy];
    if (artwork) metadata[MPMediaItemPropertyArtwork] = artwork;
    MPRemoteCommandCenter *commands = [MPRemoteCommandCenter sharedCommandCenter];
    commands.changeShuffleModeCommand.currentShuffleType = [state[@"shuffle"] boolValue] ? MPShuffleTypeItems : MPShuffleTypeOff;
    NSString *repeat = state[@"repeat"];
    commands.changeRepeatModeCommand.currentRepeatType = [repeat isEqualToString:@"track"] ? MPRepeatTypeOne
        : [repeat isEqualToString:@"context"] ? MPRepeatTypeAll : MPRepeatTypeOff;
    MPNowPlayingInfoCenter *center = [MPNowPlayingInfoCenter defaultCenter];
    publish();
    center.playbackState = playing ? MPNowPlayingPlaybackStatePlaying : MPNowPlayingPlaybackStatePaused;
    if (!playing) {
        [node stop];
        [engine stop];
    }
}

static NSURL *terminalBundle(void) {
    for (pid_t pid = getppid(); pid > 1;) {
        NSRunningApplication *app = [NSRunningApplication runningApplicationWithProcessIdentifier:pid];
        if ([app.bundleURL.pathExtension isEqualToString:@"app"]) return app.bundleURL;
        struct proc_bsdinfo info;
        if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) != sizeof(info)
            || info.pbi_ppid == (uint32_t)pid) break;
        pid = info.pbi_ppid;
    }

    NSDictionary *env = NSProcessInfo.processInfo.environment;
    NSString *program = [env[@"TERM_PROGRAM"] lowercaseString];
    NSDictionary *terminals = @{
        @"bmux": @"dev.bmux.app",
        @"[bmux]": @"dev.bmux.app",
        @"ghostty": @"com.mitchellh.ghostty",
        @"apple_terminal": @"com.apple.Terminal",
        @"iterm.app": @"com.googlecode.iterm2",
        @"wezterm": @"com.github.wez.wezterm",
        @"kitty": @"net.kovidgoyal.kitty",
        @"warpterminal": @"dev.warp.Warp-Stable",
    };
    NSString *identifier = terminals[program ?: @""] ?: env[@"__CFBundleIdentifier"];
    return identifier.length
        ? [NSWorkspace.sharedWorkspace URLForApplicationWithBundleIdentifier:identifier]
        : nil;
}

static int printTerminalIcon(void) {
    NSURL *bundle = terminalBundle();
    if (!bundle) return 1;
    NSImage *icon = [NSWorkspace.sharedWorkspace iconForFile:bundle.path];
    NSRect proposed = NSMakeRect(0, 0, 256, 256);
    CGImageRef image = [icon CGImageForProposedRect:&proposed context:nil hints:nil];
    if (!image) return 1;
    CGColorSpaceRef color = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    CGContextRef context = CGBitmapContextCreate(NULL, 256, 256, 8, 0, color, kCGImageAlphaPremultipliedLast);
    CGColorSpaceRelease(color);
    if (!context) return 1;
    CGContextSetInterpolationQuality(context, kCGInterpolationHigh);
    CGContextDrawImage(context, CGRectMake(0, 0, 256, 256), image);
    CGImageRef scaled = CGBitmapContextCreateImage(context);
    CGContextRelease(context);
    if (!scaled) return 1;
    NSBitmapImageRep *bitmap = [[NSBitmapImageRep alloc] initWithCGImage:scaled];
    CGImageRelease(scaled);
    NSData *png = [bitmap representationUsingType:NSBitmapImageFileTypePNG properties:@{}];
    if (!png.length) return 1;

    // A single 256px PNG record is sufficient for macOS's small app badge.
    uint32_t length = (uint32_t)png.length;
    uint32_t total = CFSwapInt32HostToBig(length + 16);
    uint32_t record = CFSwapInt32HostToBig(length + 8);
    fwrite("icns", 1, 4, stdout);
    fwrite(&total, 4, 1, stdout);
    fwrite("ic08", 1, 4, stdout);
    fwrite(&record, 4, 1, stdout);
    return fwrite(png.bytes, 1, png.length, stdout) == png.length ? 0 : 1;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc == 2 && strcmp(argv[1], "--terminal-icon") == 0) {
            return printTerminalIcon();
        }
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        NSString *iconPath = [NSBundle.mainBundle pathForResource:@"Terminal" ofType:@"icns"];
        if (iconPath) {
            NSImage *icon = [[NSImage alloc] initWithContentsOfFile:iconPath];
            if (icon) app.applicationIconImage = icon;
        }
        registerCommands();
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_UTILITY, 0), ^{
            char *line = NULL;
            size_t capacity = 0;
            ssize_t len;
            while ((len = getline(&line, &capacity, stdin)) >= 0) {
                @autoreleasepool {
                    NSData *bytes = [NSData dataWithBytes:line length:len];
                    id state = [NSJSONSerialization JSONObjectWithData:bytes options:0 error:nil];
                    if ([state isKindOfClass:NSDictionary.class]) {
                        dispatch_async(dispatch_get_main_queue(), ^{ update(state); });
                    }
                }
            }
            free(line);
            dispatch_async(dispatch_get_main_queue(), ^{
                clear();
                exit(0);
            });
        });
        [app run];
    }
}
