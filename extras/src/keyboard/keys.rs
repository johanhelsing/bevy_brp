//! Send-keys handler: press/hold/release key sequences via BRP.

use std::str::FromStr;
use std::time::Duration;

use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use bevy::window::WindowEvent;
use bevy_remote::BrpError;
use bevy_remote::BrpResult;
use bevy_remote::error_codes::INVALID_PARAMS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use super::constants::DEFAULT_KEY_DURATION_MS;
use super::constants::MAX_KEY_DURATION_MS;
use super::events;
use super::key_code::KeyCodeWrapper;
use crate::constants::MISSING_REQUEST_PARAMETERS_MESSAGE;
use crate::window_event;

/// One `send_keys` request: its keys, from the press until a frame after the release.
///
/// A request for a key another request still holds waits its turn instead of
/// pressing at once. A real key cannot go down while it is down: Bevy's
/// `ButtonInput` would see the second press as no change and the first
/// release would end both, so a quick second tap of the same key would be
/// lost. Requests for the same key therefore press in the order they arrived,
/// each one frame after the previous release, so the release and the next
/// press reach `ButtonInput` in different frames.
#[derive(Component)]
pub(super) struct TimedKeyRelease {
    /// The key code wrappers to release (stores wrapper for text field generation)
    pub(super) keys:  Vec<KeyCodeWrapper>,
    /// Timer tracking the remaining duration, started when the keys go down
    pub(super) timer: Timer,
    /// Where the request is between its press and its release
    pub(super) phase: KeyPhase,
    /// Arrival order, so that waiting requests press first come, first served
    pub(super) order: u64,
}

/// Where a [`TimedKeyRelease`] is in its press-hold-release cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyPhase {
    /// Another request holds one of its keys; not pressed yet.
    Waiting,
    /// Pressed, released when the timer finishes.
    Held,
    /// Released this frame; despawned next frame, which frees its keys.
    Released,
}

/// Arrival counter for [`TimedKeyRelease::order`].
#[derive(Resource, Default)]
struct KeyRequestCount(u64);

/// Request structure for `send_keys`
#[derive(Debug, Deserialize)]
pub(super) struct SendKeysRequest {
    /// Array of key codes to send
    keys:        Vec<String>,
    /// Duration in milliseconds to hold the keys before releasing
    #[serde(default = "default_duration")]
    duration_ms: u32,
}

/// Response structure for `send_keys`
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SendKeysResponse {
    /// Whether the operation was successful
    pub(super) success:     bool,
    /// List of keys that were sent
    pub(super) keys_sent:   Vec<String>,
    /// Duration in milliseconds the keys were held
    pub(super) duration_ms: u32,
}

const fn default_duration() -> u32 { DEFAULT_KEY_DURATION_MS }

/// Validate key codes and return the parsed key code wrappers
fn validate_keys(keys: &[String]) -> Result<Vec<(String, KeyCodeWrapper)>, BrpError> {
    let mut validated_keys = Vec::new();

    for key_str in keys {
        match KeyCodeWrapper::from_str(key_str) {
            Ok(wrapper) => {
                validated_keys.push((key_str.clone(), wrapper));
            },
            Err(_) => {
                return Err(BrpError {
                    code:    INVALID_PARAMS,
                    message: format!("Invalid key code '{key_str}': Unknown key code"),
                    data:    None,
                });
            },
        }
    }

    Ok(validated_keys)
}

/// Handler for `send_keys` requests
///
/// Simulates keyboard input by sending key press/release events
///
/// # Errors
///
/// Returns `BrpError` if:
/// - Request parameters are missing
/// - Request format is invalid
/// - Any key code is invalid or unknown
pub(crate) fn send_keys_handler(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // Parse the request
    let request: SendKeysRequest = if let Some(params) = params {
        serde_json::from_value(params).map_err(|e| BrpError {
            code:    INVALID_PARAMS,
            message: format!("Invalid request format: {e}"),
            data:    None,
        })?
    } else {
        return Err(BrpError {
            code:    INVALID_PARAMS,
            message: MISSING_REQUEST_PARAMETERS_MESSAGE.to_string(),
            data:    None,
        });
    };

    // Validate key codes
    let validated_keys = validate_keys(&request.keys)?;
    let valid_key_strings: Vec<String> = validated_keys.iter().map(|(s, _)| s.clone()).collect();
    let wrappers: Vec<KeyCodeWrapper> = validated_keys.iter().map(|(_, w)| *w).collect();

    // Validate duration doesn't exceed maximum
    if request.duration_ms > MAX_KEY_DURATION_MS {
        return Err(BrpError {
            code:    INVALID_PARAMS,
            message: format!(
                "Duration {}ms exceeds maximum allowed duration of {}ms (1 minute)",
                request.duration_ms, MAX_KEY_DURATION_MS
            ),
            data:    None,
        });
    }

    // Press now, addressed to the primary window, unless an earlier request
    // still holds one of these keys: then wait for its release.
    if !wrappers.is_empty() {
        let busy = world
            .query::<&TimedKeyRelease>()
            .iter(world)
            .any(|other| other.keys.iter().any(|key| wrappers.contains(key)));
        let phase = if busy {
            KeyPhase::Waiting
        } else {
            let window = events::primary_window_entity(world);
            for event in events::create_keyboard_events(&wrappers, ButtonState::Pressed, window) {
                window_event::write_input_event(world, event);
            }
            KeyPhase::Held
        };
        let mut count = world.get_resource_or_insert_with(KeyRequestCount::default);
        let order = count.0;
        count.0 += 1;
        world.spawn(TimedKeyRelease {
            keys: wrappers,
            timer: Timer::new(
                Duration::from_millis(u64::from(request.duration_ms)),
                TimerMode::Once,
            ),
            phase,
            order,
        });
    }

    Ok(json!(SendKeysResponse {
        success:     true,
        keys_sent:   valid_key_strings,
        duration_ms: request.duration_ms,
    }))
}

/// System that processes timed key releases, and presses the requests that
/// were waiting for them.
///
/// The hold runs on `Time<Real>`, not the default (virtual) clock: apps pause
/// or scale the virtual clock (a title screen, an editor pause, a replay), and
/// an injected key must still come back up so the next press is a fresh one.
pub(super) fn process_timed_key_releases(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut query: Query<(Entity, &mut TimedKeyRelease)>,
    primary_window: Query<Entity, With<PrimaryWindow>>,
    mut keyboard_events: MessageWriter<KeyboardInput>,
    mut window_events: MessageWriter<WindowEvent>,
) {
    let window = primary_window.single().unwrap_or(Entity::PLACEHOLDER);
    let mut send = |keys: &[KeyCodeWrapper], state: ButtonState| {
        // Text is only set on presses
        for event in events::create_keyboard_events(keys, state, window) {
            window_events.write(WindowEvent::from(event.clone()));
            keyboard_events.write(event);
        }
    };

    // Keys down, or released this frame: a waiting request for one of them
    // must not press yet.
    let mut busy: Vec<KeyCodeWrapper> = Vec::new();
    let mut waiting = Vec::new();
    for (entity, mut timed_release) in &mut query {
        match timed_release.phase {
            KeyPhase::Released => {
                commands.entity(entity).despawn();
            },
            KeyPhase::Held => {
                timed_release.timer.tick(time.delta());
                if timed_release.timer.is_finished() {
                    send(&timed_release.keys, ButtonState::Released);
                    timed_release.phase = KeyPhase::Released;
                }
                busy.extend(&timed_release.keys);
            },
            KeyPhase::Waiting => waiting.push((timed_release.order, entity)),
        }
    }

    waiting.sort_unstable();
    for (_, entity) in waiting {
        let Ok((_, mut timed_release)) = query.get_mut(entity) else {
            continue;
        };
        if !timed_release.keys.iter().any(|key| busy.contains(key)) {
            send(&timed_release.keys, ButtonState::Pressed);
            timed_release.phase = KeyPhase::Held;
        }
        // Pressed now or still waiting, its keys are taken for this frame.
        busy.extend(&timed_release.keys);
    }
}
