# Remote Monitor and Remote Input

This native Amaterasu plugin supplies per-client session controls and catalog
presentation through the public `nonary.session-presentation/1` SDK contract.
The NVHTTP consumer uses that same versioned contract with replacement providers.
The plugin does not implement its own protocol listener, input injector, display
driver or encoder.

Remote Monitor keeps a client-owned display separate from the running
application. Remote Input streams a composed black capture source and routes
authenticated input without acquiring a host display or audio capture. The
synthetic Resume, Disconnect and Terminate tiles are protocol controls; they are
not catalog applications. The plugin projects the app list and reported running
application for the authenticated client so Moonlight can select these controls.
Launch authorization is evaluated again when a control is used.

## Build independently

The plugin requires a current Amaterasu SDK checkout containing the session
presentation contract. An SDK dependency path is resolved by package identity;
the build helper does not rewrite either source checkout. Set the target
directory to a build volume before running it:

```sh
export CARGO_TARGET_DIR=/absolute/build-volume/remote-session-target
python3 scripts/build-native.py /path/to/amaterasu --test
python3 scripts/build-native.py /path/to/amaterasu --clippy
python3 scripts/build-native.py /path/to/amaterasu --fmt
python3 scripts/build-native.py /path/to/amaterasu --release
cd ui
npm ci
npm test
npm run build
```

When checked out at `modules/remote-session`, the parent Cargo workspace builds
the same native library as `amaterasu-remote-session`. The independent
`amaterasu-capture-black` component supplies black pixels through the standard
typed Capture SDK. Neither component requires a different host executable.

## Trusted composition

The product package profiles select the native plugin explicitly and set
NVHTTP's `session_presentation_provider` to the public contract service. The
selected provider and allowed caller identities are approved in package policy.
All SDK method schemas must match the installed approvals exactly. Removing the
selection uses the protocol consumer's ordinary application behavior; failure
of a selected provider is reported as an error.

`auxiliary_black_capture_provider` selects the composed `black` capture alias.
`auxiliary_exact_capture_provider` selects the capture provider that can verify
the retained display's exact output. Linux profiles use their selected `vkms` or
`virtual_screen` route; Windows uses `dxgi`. A retained display is never replaced
with an arbitrary active desktop or portal selection.

The `auxiliary_exact_capture_routes` map selects providers for explicit semantic
route keys. `managed_output` uses the retained-display provider. Windows also
selects `dxgi` for `desktop_output`. Linux exposes `desktop_output` only when the
signed startup policy separately authorizes existing KWin output capture. Build
PipeWire with
`amaterasu-capture-pipewire/linux-kwin-screencast` and use the existing
`init-linux-session --enable-capture --enable-desktop-output-capture` options
to make that route available. Managed `allow_named_outputs` permission does not
authorize physical outputs; the new option sets `allow_desktop_outputs` instead.
Packaging verifies the physical-capture feature in the exact artifact, and the
consumer still probes the exact selected capture provider before application
effects. A configured map alone
does not establish compiled or runtime availability.

Native plugins are trusted code. API grants restrict the host's broker routing;
they are not a native-code sandbox. Existing input, capture, display mutation,
remote-listener and publisher permissions remain in force.

## Settings and signed UI

`metadata/api.json` defines the public `nonary.remote-session/1` methods:
`settings_get`, revision-checked `settings_update`, `layout_get`, `layout_update`,
`profile_targets`, `profiles_get`, `profile_upsert` and `profile_delete`. Only
product preferences, monitor placements and marked monitor profiles are editable
through this API.
Bootstrap `enabled`, `allowed_principals`, `source_verifiers`, capture provider
selection and trust policy are not exposed as user settings.

| Preference | Default | Behavior |
| --- | --- | --- |
| `remote_monitor_mute_audio` | `false` | Suppress audio sent to Remote Monitor. Host playback is retained independently. |
| `remote_monitor_disconnect_on_stream_end` | `false` | Release the client's retained monitor when its stream ends. |
| `remote_monitor_disconnect_on_client_disconnect` | `false` | Release the retained monitor after client transport loss. |
| `remote_monitor_terminate_on_first_request` | `false` | Allow a secondary client to terminate the running application with one request. |
| `remote_monitor_confirm_app_replacement` | `true` | Require confirmation before replacing the running application. |
| `show_remote_input` | `true` | Advertise the Remote Input control when permitted. |
| `show_remote_monitor` | `true` | Advertise the Remote Monitor control when permitted. |
| `max_clients` | `16` | Bound retained client ownership; the supported range is 1–32. |
| `remote_monitor_layout` | Empty version 1 layout | Save per-client monitor placement relative to another client or a physical output. |

The Monitor layout form edits relative anchors, edge, alignment, gap and the
preferred primary output. The provider validates cycles and missing anchors and
reports whether the saved layout is applied, waiting for a monitor or requires
attention. Layout and preference changes share an optimistic settings revision.

The Monitor profiles form creates and edits inert catalog records with their
own stable identity, mode, HDR, scale and target settings. A target can be a
client-owned display or a ready existing output selected from the provider's
opaque target inventory. An owned monitor uses the selected mode and scale.
An existing display keeps its observed mode; the profile selects the requested
stream mode, and scale does not alter that display. These records share the
protocol catalog's existing owner and revision; the plugin does not maintain a
second hidden catalog. Editing a profile affects future acceptance. A retained
monitor uses its frozen accepted profile until it is disconnected.

Profile lists use revision-bound pages within the broker payload limit. The UI
collects the complete snapshot before replacing its editor state and offers a
searchable local picker for large catalogs. A failed or stale continuation keeps
the previous snapshot and unsaved changes.

The independent UI uses approved public API aliases through the shared UI SDK.
Build its assets, then prepare a bundle with an already approved publisher key:

```sh
scripts/package-ui.sh /path/to/amaterasu-web-extension OUTPUT SEQUENCE EXPIRES_UNIX SECRET_HEX_FILE
```

Select the signed extension with id `remote-session`, its dedicated extension
root, pinned publisher key and the exact method declarations from `ui/ui.json`.
Signing a UI bundle does not grant its methods or activate the native package.
Use the normal signed repository import and authenticated activation flow for
native package installation; see the parent project's local package update
documentation. Building this repository does not install or publish it.

## Qualification

See the parent project's `docs/remote-session-plugin.md` for the behavior and
verification boundaries. Stateful provider, protocol and composition tests cover
source behavior. Native display creation/restoration, real client reconnects,
controller feedback, HDR and playback require qualification on each target
platform. A retained ownership record alone is not proof of a native resource,
and successful input admission is not proof that a remote client received data.

The stock Windows MFT encoder supports NV12/H.264 and cannot encode ten-bit SDR
or HDR. Higher-depth capture and capability-based routing permit a capable SDK
replacement encoder; they do not add HDR to that stock encoder. The direct
Linux KWin PipeWire CPU path remains BGRA8, so HDR physical-output capture is
also unsupported there. These are source capability limits, separate from
native hardware/client qualification.

The parent repository also supplies `scripts/test_remote_session_plugin.py` for
isolated signed native qualification. It uses a fresh test publisher and the
in-memory input fixture, with display, desktop capture and audio effects
disabled. Its full mode decodes the black stream and checks held-input cleanup;
`--loader-only` checks module loading and contract identity. Both operate on
explicit build artifacts and a private build-volume test root, never an
installed service or publisher key.
