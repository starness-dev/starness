# Complete Harel / XState v5 feature showcase:
# 1. `parallel_state` fork-join (`on_done` fires automatically when both regions finish)
# 2. `state_in(S.audio_ready)` cross-region guard
# 3. `shallow_history` (`H`) resuming exact substate (`playing` vs `buffering`) after `PAUSE`
# 4. `forbid(event = ...)` blocking parent superstate transitions while buffering
# 5. Built-in actions (`emit_event`, delayed `raise_event`, `cancel_send`, `log_action`) + `reenter = True`
# 6. `tags` + `final_state(output = ...)`

MediaEvent = enum(
    "AUDIO_OK",
    "VIDEO_OK",
    "STALL",
    "BUFFERED",
    "BUFFER_TIMEOUT",
    "SEEK",
    "PAUSE",
    "RESUME",
    "STOP",
)
Tag = enum("loading", "active")

S = state_space(
    "booting",
    "audio_region",
    "audio_init",
    "audio_ready",
    "video_region",
    "video_init",
    "video_ready",
    "playback",
    "playing",
    "buffering",
    "playback_hist",
    "paused",
    "finished",
)

chart: StateMachine = machine(
    id = "media_player",
    description = "Full Harel statechart with parallel fork-join, history, state_in, and built-in actions",
    initial = S.booting,
    output = {"result": "session_ended"},
    states = {
        S.booting: parallel_state(
            tags = [Tag.loading],
            on_done = outcome(
                target = S.playback,
                actions = [emit_event(event = "PLAYER_READY", payload = {"hd": True})],
            ),
            states = {
                S.audio_region: compound_state(
                    initial = S.audio_init,
                    states = {
                        S.audio_init: state(
                            on = [transition(event = MediaEvent.AUDIO_OK, target = S.audio_ready)],
                        ),
                        S.audio_ready: final_state(),
                    },
                ),
                S.video_region: compound_state(
                    initial = S.video_init,
                    states = {
                        S.video_init: state(
                            on = [
                                transition(
                                    event = MediaEvent.VIDEO_OK,
                                    target = S.video_ready,
                                    guard = state_in(S.audio_ready),
                                ),
                            ],
                        ),
                        S.video_ready: final_state(),
                    },
                ),
            },
        ),
        S.playback: compound_state(
            initial = S.playing,
            tags = [Tag.active],
            on = [
                transition(event = MediaEvent.PAUSE, target = S.paused),
                transition(event = MediaEvent.STOP, target = S.finished),
            ],
            states = {
                S.playback_hist: shallow_history(target = S.playing),
                S.playing: state(
                    entry = [log_action("entered_playing")],
                    on = [
                        transition(event = MediaEvent.STALL, target = S.buffering),
                        transition(event = MediaEvent.SEEK, target = S.playing, reenter = True),
                    ],
                ),
                S.buffering: state(
                    tags = [Tag.loading],
                    entry = [
                        raise_event(
                            event = MediaEvent.BUFFER_TIMEOUT,
                            delay = 500,
                            id = "buffer_watchdog",
                        ),
                    ],
                    exit = [cancel_send(id = "buffer_watchdog")],
                    on = [
                        forbid(event = MediaEvent.STOP),
                        transition(event = MediaEvent.BUFFERED, target = S.playing),
                        transition(event = MediaEvent.BUFFER_TIMEOUT, target = S.finished),
                    ],
                ),
            },
        ),
        S.paused: state(
            on = [transition(event = MediaEvent.RESUME, target = S.playback_hist)],
        ),
        S.finished: final_state(
            output = {"result": "session_ended"},
        ),
    },
)
