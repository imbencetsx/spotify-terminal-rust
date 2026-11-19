#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#import <MediaPlayer/MediaPlayer.h>

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

int main(void) {
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
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
