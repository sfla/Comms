# Local microphone mute (Windows)

Connect to a server, then click **Configure shortcuts** beside your own name.
The list starts empty. **Add shortcut** creates a row containing an action,
a binding button, and **Remove**. Click the binding button and press one keyboard
key or mouse button. Escape cancels recording. Changes save automatically to
`shortcuts.json` beside Comms' existing local settings.

- **Toggle mute** switches microphone transmission on/off once per press.
- **Hold mute** stops transmission while its key/button is held.
- Multiple bindings per action are allowed, including Mouse 5 and CapsLock.
- With overlapping holds, all held bindings must be released before transmitting.
- A toggle mute stays in effect after releasing a hold. Toggling off during a hold
  resumes transmission only after all holds are released.
- The microphone button next to your name also toggles mute. It is disabled while
  a Hold mute binding is held to avoid implying that it can override that hold.
- Binding recording does not execute shortcuts. Normal keyboard/mouse behavior
  is retained (for example, CapsLock still controls Windows' caps-lock state).
- Bindings work globally, including when Comms is unfocused. This version binds
  individual keys/buttons; combinations and mouse-wheel bindings are not included.

Mute only affects your microphone. Incoming voices and their volume controls
are unchanged. Microphone samples are discarded while muted, and pending audio
commands from before a mute transition are invalidated. This also gates diagnostic
raw-microphone uploads. Already transmitted audio cannot be recalled.

Mute state is local, starts unmuted on launch, and survives disconnect/reconnect
within the running app. Only bindings persist across launches. Other clients do
not see the mute indicator, and no server update is required.

Windows secure desktops (such as the lock screen) do not deliver global shortcut
events. If you release a hold there, press and release that binding again after
returning to Comms; the missing release conservatively leaves the microphone muted.
Hook setup errors appear in the configuration window; the microphone button
remains available.

## Validation

On a configured Windows build machine:

```powershell
cargo fmt --all -- --check
cargo test --locked -p comms shortcuts::tests
cargo build --locked --bin comms
```

With two clients connected to a test server:

1. Add Hold mute on Mouse 5 and CapsLock. Hold both, release one, then the other;
   the listener should hear you only after both have been released.
2. Add Toggle mute on another key. Keep it pressed to check that key repetition
   does not toggle repeatedly. Mix toggle and hold to check the behavior above.
3. Repeat with a different app/game focused and Comms minimized. Check both mouse
   side buttons and test elevated/full-screen games on the actual target PC.
4. Change, record, cancel, and delete bindings, including an active hold. Reopen
   Comms and confirm that bindings persisted, but mute starts off.
5. Speak while muted. Confirm the local muted indicator, absence of the speaking
   highlight, no microphone audio at the listener, and continued incoming voices.
6. Disconnect/reconnect while muted; confirm mute remains in effect. Switch/lock
   the Windows session while holding mute and verify safe recovery.

Mac validation: isolated tests of the shared mute/shortcut state and Windows-target
Rust type checks of the shortcut engine, hooks, and settings UI. A full Windows
build and runtime audio/UI validation still require Windows and native libraries.

Additional integration type check: all client source was checked for the Windows
target using an isolated manifest with native TLS for the update HTTP client
(in place of rustls, which requires Windows C headers on this host). GTK/Opus
were declared through build metadata without linking native Windows libraries.
This validates Rust types, not a runnable Windows binary.
