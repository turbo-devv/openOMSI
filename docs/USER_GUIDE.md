# User guide

How to run openOMSI, drive, use the launcher, install mods and play over LAN. For building
from source see [BUILDING.md](BUILDING.md).
For OpenXR headset setup and controls on Windows, see [VR.md](VR.md).

> openOMSI runs on the content of an **original OMSI 2 installation**. Without one the game does not start.

## Checking an installation

```bash
cargo run --release -p omsi-check -- "/path/to/OMSI 2"
```

This loads every content file in the install with the new loaders and reports what failed.

## Running

```bash
openomsi --root "/path/to/OMSI 2"
```

`--root` is only needed once: the path is remembered, so afterwards the program can be
started with no arguments at all. Without it the installation is looked for in `$OMSI_ROOT`,
next to the program (an `OMSI 2 Original` or `OMSI 2` folder beside openOMSI) and in the
usual Steam locations.

Started without arguments the program opens the launcher (see below). `--menu` shows the
in-game start menu instead, which asks for map, vehicle, time, traffic, passengers, the
timetable, the weather and the date (arrow keys change values, Enter starts, Esc quits).
Everything can also be given on the command line, which then skips both:

| Flag | Meaning |
| --- | --- |
| `--map maps/Grundorf/global.cfg` | map to load |
| `--weather Weather/Schmuddelwetter.owt --date 1989-01-15` | weather, and the date that decides the season |
| `--bus Vehicles/MAN_SD200/MAN_SD80.bus` | player vehicle (`--paint name`, `--hof name`, `--plate "B-AB 1234"`) |
| `--entry N` / `--spawn x,y,heading` | where the vehicle starts |
| `--time HH:MM --date YYYY-MM-DD --weather Weather/x.owt` | time, date, weather |
| `--traffic N --schedule --line 76 --tour 1 --passengers` | AI cars, timetable buses, the player's tour, people at the stops |
| `--radius N` / `--all` | tiles around the start / the whole map |
| `--view-distance M` | how far around the camera the window keeps tiles loaded (m, 1200 by default) |
| `--content-zip mod.zip` | read an archive in place as a content root (repeatable; `OMSI_CONTENT_ZIP` does the same) |
| `--offscreen out.png --cam x,y,z,yaw,pitch --drive secs` | render one frame to a file |
| `--snapshots 7,9,11` / `--follow auto\|bus\|moving\|type:X` | more pictures during a `--drive` run, camera behind an AI vehicle |
| `--riders N --refuel --wash --repair --dirt 0..1` | passengers already aboard, the depot services, how dirty the bus starts |
| `--driver Drivers/OMSI-Fan.odr` | the driver's personnel file; the run is added to it |
| `--drive-keys wasd` | let W/A/S/D drive instead of the arrow keys |
| `--autostart` | put the bus into service before the run (as Shift+U does) |
| `--click x,y[,dx,dy]` | press (and drag) the cockpit switch at that pixel, offscreen |
| `--season winter` / `--situation x.osn` / `--physics simple` | season override, a saved situation, the kinematic dynamics instead of the rigid body |
| `--enhanced` / `--export-glb bus.glb` | the physically based renderer; write the bus as glTF (the launcher's preview) and quit |
| `--launcher` / `--menu` / `--no-menu` | open the launcher (the default without arguments), the in-game menu, or neither |

Keys in the window: **W** throttle, **S** brake, **A**/**D** steering - the arrow keys do the
same - and every vehicle key of `Inputs/keyboard.cfg` works as it does in OMSI: throttle
Shift+Num 8, brake Shift+Num 2, steering Shift+Num 4/6, **E** battery and ignition, **M**
starter, **N**/**R** the automatic, **.** the parking brake. W, S and D are OMSI's wiper,
viewpoint and **D of the automatic gearbox**, so hold shift for those: **Shift+D** selects D.
`--drive-keys arrows` leaves W/A/S/D to OMSI entirely.

To start a stock SD200: **E** (battery - it puts the ignition key in as well), **M** held for a
second (starter), **Shift+D** (drive), **.** (parking brake off), then throttle. **Shift+U** does the
whole start-up by itself (main switch, ignition, starter, gearbox to neutral); `--autostart`
is the same thing for an offscreen run.

**Updates.** When the launcher starts it asks
[github.com/openOMSI-Project/openOMSI](https://github.com/openOMSI-Project/openOMSI) for the latest release
and, when there is a newer one, offers it: **Update now** downloads it (checked against the
SHA-256 GitHub lists), puts the new program in place of the old one and starts the launcher
again - on Windows `openomsi.exe` and `openomsi-launcher.exe`, on macOS the `openOMSI.app`
you started, on Linux the program files; mods, content and settings stay. On Android the
system's installer asks "Do you want to update this app?"; Update replaces openOMSI and starts
it again, Cancel leaves it as it was. Settings → General → Updates: look for updates at the start (on
by default), install without asking (off by default), Check now. A folder openOMSI cannot
write to (Program Files, an app opened straight from Downloads on macOS) is reported with
what to do. `OMSI_NO_UPDATE=1` switches the check off; `OMSI_UPDATE_URL` points it at another
release description (GitHub's format; `file://` works, for testing).

**O** switches mouse steering on and off, as in OMSI (Omsi.exe's own formula): the cursor's
place across the whole window is the steering from full left to full right lock
(`[inv_min_turnradius]` of the bus), from the middle up to the top edge is the throttle and
down to the bottom edge the brake. Above 10 km/h the same hand movement turns the wheels less
and less (at 50 km/h a fifth as far), so the wheel feels heavier the faster the bus goes; for
the first second after switching it on the wheel and the pedals ease towards the cursor.
Settings → Driving → *Mouse steering sensitivity* makes it more or less sensitive (100 % = OMSI).
Mouse steering works in the driver's, the passenger and the outside view; the wheel follows
the cursor smoothly (a short easing, no steps).

Two switches there change the steering keys (both off by default): *Steering linearity* turns
the wheel at OMSI's own steady pace (the curvature grows by the same amount every millisecond
the key is held, whatever the bus), and *Old Steering* is OMSI's wheel that stays where you
leave it - turn it back yourself. The clutch key works as in OMSI: the pedal goes down at once
and comes up slowly (0.7 per second) when the key is released.

Left-click a cockpit switch to operate it, hold the button and move the mouse to turn a knob,
or roll the mouse wheel over it (that is the `<event>_drag` OMSI fires); the name of the switch
under the cursor is shown in the HUD.
Right-drag the mouse (or drag with the wheel pressed, OMSI's pan) to look around in any view
(the head turns inside, the camera swings around the bus outside), I/J/K/L does the same from
the keyboard, and Shift+right-drag zooms - this is OMSI's `[altView]` mode, the Camera setting
"Right mouse button turns the view". Switched off, the right button zooms as in OMSI's default
(up: the outside camera backs away, the view inside widens up to the seat's own) and only the
wheel button turns the view; each view keeps its own
direction (turning the outside camera leaves the driver's head where it was), **Space** looks
ahead again in every view (OMSI's `view_reset_all_directions`), Home recentres the view shown
where keyboard.cfg does not make it the ticket desk camera.
The mouse wheel (and **=** / **-**, a pinch on a phone) zooms: outside the camera comes closer,
inside the bus the view narrows, as in OMSI; **Ctrl**+wheel outside narrows the view instead
(a telephoto, the camera stays where it is). F1-F4 driver / passenger / outside / map (free) camera, F5-F8 the destination
sign and roller blind keys as in OMSI, Ctrl+S quick save, F9 write the run into the personnel
file, WASD+QE in the free camera (**Ctrl**+click on the ground there moves the bus to the
nearest street), left click on cockpit elements, **V** the chat line in a
LAN session. Esc opens the game menu: drive the next placed vehicle, place any vehicle of
the installation in front of the camera (or beside the bus), couple what stands close behind
the bus and uncouple it again, save the situation or load the quicksave, the next weather, the clock an hour on or back, refuel and wash (only at a
petrol station, as in OMSI), repair (the team needs the map's travel time when the bus stands
in no depot yard), screenshot, timetable, the object editor (below), quit. Its *Options* hold
one line a setting under the launcher's headings (Simulation, Display & sound, Driving,
Camera): **Left** and **Right** (or a click on the arrows round the value) step it down and
up, Enter as before; they are kept for the next game. Home is the ticket desk camera and Insert the timetable view (as OMSI's keyboard.cfg binds them), and the
change keys of keyboard.cfg hand out or take back the change. The HUD
shows time, speed, line, next stop, delay and what the workshop just did (and why the bus
stands: the parking brake, low air pressure, a line the date's chrono takes off), and the
controls for the first seconds.

## The launcher

The launcher is the game's own window (`crates/omsi-app/src/launcher`): `omsi` started
without arguments (or with `--launcher`) opens it. It is drawn with wgpu - no web engine -
flat and dark (neutral greys, one amber accent), every control custom (sliders, switches,
dropdowns, a calendar, a time picker, text fields, segmented buttons), Material Symbols
icons and Roboto (`crates/omsi-ui`). The Drive page shows the chosen bus in a card, as a
picture **drawn by the game's renderer** - its model, paint, materials, reflections and
shadows exactly as in the game, under the light of the chosen time and weather - drawn
again only when something changes; drag on it to turn the bus, scroll to zoom. Its pages:

* **Drive** - four steps: the bus (search, liveries, depot file, number plate), the route (map, start
  point, line and tour - the lines that run on the chosen date), time and weather (time,
  date, season, traffic, passengers, timetable buses, autostart, *LAN play: host / join*,
  the weather presets that suit the season), and the roadbook with the IBIS codes; the
  summary and **Start the duty** bottom right.
* **Profile** - hours, experience and level, from OMSI's own `.odr` personnel files plus
  the session summaries the game writes to `~/.openomsi/sessions`.
* **Settings** - everything in `settings.cfg` below, saved as it changes; keys the page
  does not manage are kept as they are. One tab for each thing one comes to change:
  *Graphics* (the quality preset first, the screen, distances and memory), *Driving* (keys,
  mouse, wheel and pedals, with the way to the Controls page), *Camera* (the seat, the views,
  head tracking, VR), *Sound*, *Gameplay* (passengers, traffic, collisions, the clock) and
  *General* (language, the game's interface size, navigator, Discord Rich Presence,
  updates, and resetting every setting).
  **Discord Rich Presence** shows the launcher while preparing a drive, then the map and
  line above the vehicle type and tour while playing. The full vehicle name is in the logo's
  tooltip. The launcher status returns when the game ends.
  It is enabled by default and can be turned off under Settings → General; the switch
  affects the launcher immediately and the game on its next start. Discord must be running
  on the same computer.
  **Voice chat through GreenTeaSpeak** (on by default): in multiplayer, the other players
  are heard from where they are, when GreenTeaSpeak 2 runs with the openOMSI plugin and the
  server names a voice channel (see [SERVER.md](SERVER.md) and
  `tools/greenteaspeak-plugin/README.md`).
* **Controls** - `Inputs/keyboard.cfg`: click a key, press the new one; clashes are red. The
  keys are the game's with *Driving keys: Custom controls* (Settings → Driving); with a ready-made
  layout (W A S D, arrows) those keys drive and win over the list - the page says so, and
  changing a key switches to Custom controls by itself. *Game controllers*: wheels, pedals,
  joysticks and button boxes as in OMSI's `gamectrler.cfg` - a connected device not set up
  yet has **Set up**, and **Set up step by step** finds its axes (turn the wheel left, press
  each pedal); every button of the device is listed (press one to jump to it). On Windows the
  devices are read through DirectInput, as OMSI does, so wheels Windows lists can be
  configured for steering. Force feedback needs a driver that supports constant force:
  parking resistance eases as the bus rolls, with centring and
  feedback from the bus's sideways acceleration, short bumps when the front wheels cross
  an edge, plus the scripts' shaking, `FF_Vib_Amp`. A wheel nobody has set up steers with
  its X axis. A wheel that a community controller mapping also makes a gamepad (a
  Logitech G29) is listed once, and *Use this device* switches any device off
  (it is then neither read nor listed as steering). Select a device to adjust *Steering force*
  (centering and resistance) and *Vibration* separately, then press **Save**. The values are stored
  for that device in the content folder's `Inputs/gamectrler.cfg`; restart a running game to use
  the new values. *Force feedback and vibration* in Settings → Driving remains the global on/off switch.
* **Sessions** - every game started from the launcher, with its log, a **Stop** that lets
  it save its run (SIGTERM, up to 8 s, and only a stuck game is killed) and, for a LAN
  session, the code to copy, who is playing and the chat.
* **Mods** - installing mods and archives (see *Mods and the content folder*); a folder or
  .zip, .7z or .rar dropped on the window is installed.
* **Timetable** - a map's lines, their tours and trips. Changes stay while you move between
  lines and are saved together (*Save all*); **New line** makes a line, **Repeat** turns a tour
  into a whole day of them (every *n* minutes up to a last departure).
* **Setup** - where the original installation and the game binary are. The OMSI 2 folder may
  be given as a path with quotes, as `Omsi.exe` itself or as a folder inside it; unpacked into
  the OMSI 2 folder itself, openOMSI keeps its own content in an `openOMSI` folder there and
  never writes to the game's.

```bash
scripts/build-macos.sh   # or build-windows.cmd / build-linux.sh: the game opens the launcher
```

Without a person at it: `OMSI_LAUNCHER_PAGE=drive:2` opens a page (and a Drive step;
`settings:3` or `controls:1` a tab), `OMSI_LAUNCHER_SHOT=secs:file.png` writes a picture,
`OMSI_LAUNCHER_EXIT=secs` closes it,
`OMSI_LAUNCHER_INPUT="t=2 click 412,60; t=3 type 76; t=4 key Enter; t=5 shot a.png"` works
it (logical pixels). The data side is `crates/omsi-launcher-core`:
`openomsi-launcher --cli lines '{"map":"maps/Grundorf/global.cfg"}'` runs any of its commands
from a terminal - `config`, `maps`, `vehicles`, `weather`, `lines`, `ibis`, `profiles`,
`profile`, `mods`, `modinfo`, `install`, `instances`, `stop`, `log`, `join`, `settings`,
`save_settings`, `keybindings`, `save_keybindings`, `preview`, `args`, `launch`. `lines`
takes a `"date":"YYYY-MM-DD"` as well: the chrono folders active that day add and remove
lines, as in the game, whose default date is 1989-05-30.

## Settings, enhanced graphics, the navigator

`~/.openomsi/settings.cfg` (written by the launcher's settings page, or by hand) holds
`msaa` (1/2/4/8; a count the GPU cannot do falls back to the next lower one), `anisotropy`
(1..16), `ssao`, `shadows`, `shadow_size`, `shadow_blobs` (the models' `[isshadow]` shadow
meshes, OMSI's flat blob under a vehicle, laid on the road its wheels stand on; off, only the
sun shadow map shades under a vehicle), `navigator`, `ui_opacity` (how much of the interface's backgrounds shows - the navigator's, the
menu's, the timetable's, the plates under the notes - 0.2 to 1, the texts staying solid; 0.85
as designed; `navigator_opacity` in older files),
`navigator_corner` (`bottom-left` default, `bottom-right`, `top-left`, `top-right`),
`nav_ai` (the other AI vehicles as dots on the navigator and the city map; on by default),
`boarding`, `detail_textures`, `exact_fare`, `enhanced`, `fullscreen`, `vsync`, `volume`
and `drive_keys`, plus `render_scale` (`auto` or a fraction: the picture is drawn smaller
and upscaled), `post_aa` (`fxaa`, the enhanced renderer's, or `off`), `view_distance` (m,
how far the tiles are kept loaded), `texture_memory` (MB - OMSI's `texmemlimit` is read
under that name too; an eighth of the machine's memory when unset), `texture_compression`
(BC1-BC3 on the GPU, on by default), `reflections` (the materials' reflection maps,
`[matl_envmap]` - off, paint, chrome and glass mirror nothing), `led_glow` (0..15: how
bright an LED destination matrix's dots burn in the enhanced picture, 0 = off - they are
the panel's own light, and the glow draws a halo around them), `led_mips` (0..4, 0.05 steps,
1.3 by default: how much of the mip chain an LED matrix is held at - its picture and its
`\S:n` mask are sampled at the level their screen footprint asks for, never coarser than
this. 0 point-samples them, the sharpest dots and the worst shimmer; 1.3 keeps a matrix's
dots a couple of pixels across where the full chain has run them together; 4 is near the
calm of the full chain), `mouse_sens` (mouse steering,
1 = OMSI's), `ui_scale` (the size of the game's interface over the picture - its texts,
the menu, the timetable, the navigator and the city map - from 0.5 to 2, 1 by default, on
top of the screen's own scaling; on a window taller than 1080 lines the interface grows with
it as well, up to twice, unless `ui_scale_window` is off; `notes` off hides the notes in the
top left corner; *Options* in the game menu changes it in quarters while driving, Left
and Right), `steering_linear` and `old_steering` (the two steering switches above),
`ff_invert` (force feedback the other way round), `wheel_range` (the wheel's own rotation,
lock to lock, 900° by default) and `wheel_lock` (how far it is turned for the bus's full
lock; 0 = the whole wheel, as OMSI), `fov` (degrees for the views from the bus; 0 = the bus's
own cameras), `head_idle` (0..1, 0 by default: how far the view sways on its own - a head at
rest breathes and shifts its weight, and 0 of it is exactly as OMSI; most of it is seen while
the bus waits at a stop) and `head_idle_pace` (0.5..2: how fast that sway moves, 1 being the
pace it is designed at), `collision_objects` (walls, poles and bridges stop the bus; off is OMSI's `no_collision`
option and is taken from OMSI's options when openOMSI starts the first time), `graphics_api`
(`auto`, `vulkan`, `dx12` on Windows, `gl`: which graphics interface the game asks first -
with `auto` Vulkan, then DirectX 12, then OpenGL), `ctrl_off` (game controllers switched off
on the Controllers page, by name, separated by `|`) and `language` (`ENG`, `DEU`, `FRA`: the language the HUD names cockpit switches
in). The file also carries a `version`; older files that say
`boarding=pay` because that was the launcher's old default are read as `auto`.

`drive_keys` is a control preset: `simple` (W/S/A/D and the arrow keys drive; the default),
`wasd`, `arrows`, or `omsi` ("Custom controls") - only the layout of `Inputs/keyboard.cfg`
(OMSI's Shift + numpad, or what the Controls page made of it), nothing added. **T** sells the ticket a passenger asks for on a bus without a
ticket printer (the original's `ticket_give` key).

`boarding` is how passengers board: `auto` (default) - they walk to the standing place the
cabin's `[ticket_sale]` names, turn to the driver, put the money down, take their ticket by
themselves after a moment and walk on; `pay` - they wait for the driver to sell the ticket
(the bus's printer, or **T**) and give up after 25 s; `walk` - straight into the saloon,
no cash desk (flat fare / ticket machines). People keep a body's width apart outside.
`exact_fare=0` makes them overpay so that change is due. Rain and snow stay outside the
player's bus (its `[boundingbox]`), and heavy rain darkens the day enough for the saloon
lights to matter.

`detail_textures` lays procedural (fractal) grain over the ground and the roads up close,
in vanilla and enhanced alike. `enhanced=1` (or `--enhanced`) switches to its own
physically based renderer: high-range lighting with energy-conserving diffuse and GGX
reflections (roughness from `[matl_envmap]`), a computed sky (Rayleigh/Mie scattering,
lit cumulus) that also lights the scene, contact-hardening sun shadows, aerial perspective
and height fog, automatic exposure, a glow only real highlights produce and the PBR
Neutral tone curve with FXAA (`post_aa`); no light shafts, vignette or grading.

Vanilla, Vanilla+ and Enhanced reflect buses, buildings and scenery in wet road puddles
when `reflections=1`, each using its own lighting. Depth-aware filtering softens the image;
Enhanced also shades shallow rain ripples.
The player's nearby bus and up to three coupled sections use one local geometry capture,
mirrored around the actual road face's height and slope. Its windows are shaded from the
reflected eye, and an open legacy chassis gets a dark underside in that same depth-tested
view. This avoids mixing offset screen-space and geometry projections on the bus.
Other objects use the current frame's colour and a private hit-depth texture that includes
reflective windows. From inside the bus, its own panes let the rays reach the street;
glass tint and rain films attenuate the reflection along with the scene behind them.
Vanilla blends wet-road reflections and fog in the original encoded colour space.
Rain drops refract a separate, full-resolution copy of the current scene, including
its puddle reflections, so wet glass and moving wipers do not feed back into later frames.
Rays run at half resolution, capped at 518400 pixels and 48 steps;
the local bus capture has the same pixel cap and a 60 m distance limit. Dry roads,
snow-covered roads and mirror views skip these passes. Reflections beyond the local road
plane use screen-space rays; objects unavailable to those rays keep the sky reflection.
OpenGL uses the sky reflection too.

The vanilla look stays the default. The navigator (`crates/omsi-app/src/navigator.rs`) sits in a corner of the screen (lower
left by default), after the Route Advisor of Euro Truck Simulator 2: small, dark and half
transparent, a tilted 3D map that turns with the bus and zooms out with speed - the roads
of the lane network, the trip's route with arrows along it, coloured stretch by stretch by
how busy the road is (blue empty, green light, yellow busy, red heavy, dark red jammed; it
changes as the traffic does), the stops ahead, the other vehicles as blue dots, the next
turn with its distance and the street it turns into, and the street the bus is on. It
routes on the whole map's road network, read from the tile files in the background at the
start, so the way shows however far the route or the next stop is from the loaded tiles.
From the depot it leads at once to the next stop (never past it onto a later
part of the route); leave the route and it is recalculated after two seconds (Dijkstra
over the lanes, back onto the route ahead). A duty begins with the first trip of the tour
whose first stop the bus can still reach before it leaves - not with the trip under way
at the start time, which put the driver late and halfway along the line. Above the map the speed with the limit, the line and the time;
below it the next stop, its distance, the time to it, its planned time and whether the bus
is early or late - in English, German, French or Russian (`language`). A click on the navigator (or **Shift+M**) opens the city map: a large window over the game
with the whole map from above - every road with its street name (read from the map's
street name signs), the trip's route by traffic with arrows, its stops with their times,
the bus and the traffic; drag to move, the wheel zooms, the buttons centre on the bus and
zoom, Escape or a click outside closes it. **Shift+N** cycles
map → map with the schedule of the next stops → off (N alone is the gearbox's neutral);
`OMSI_DEBUG_NAV=1` logs it.
**Z / X / C** are the indicators. Controls also offers **Indicator left (toggle)** and
**Indicator right (toggle)** for keyboard keys or wheel buttons such as shift paddles.
They start unbound: one press turns that side on, another turns it off, and pressing the
other side switches direction. A script's automatic cancellation is respected.
**Shift + 1**, **Shift + 2**, … open or close a door, front
to back: a bus like the SD200/SD202/EN92 with one two-leaf front door and a combined
aft/stop-brake-release door answers to Shift+1/2/3, a low-floor mod with three or four
independent doors (the O530 Facelift) to Shift+1 through Shift+4/5 - whatever
`bus_doorfront<n>` triggers the bus's own script defines, `bus_dooraft` last (the HUD's
control reminder says how many).

In Settings → Camera, **Driver's view turns with the steering** smoothly turns the driver's
view into the steering direction, independently of the bus's head-motion simulation.
**Steering view angle** sets the full-lock rotation (0–60°, default 30°), and **Steering
view response** sets the smoothing time (50–1000 ms, default 250 ms; larger values follow
more slowly). Manual looking remains available. The automatic turn is suppressed while
VR or an active head tracker controls the view. It is off by default.

## Mods and the content folder

The folder of the game binary (`dist/<platform>` in a build; beside `openOMSI.app` on macOS) is laid out like an OMSI 2
installation - `Vehicles`, `maps`, `Sceneryobjects`, `Splines`, `Texture`, `Fonts`,
`Plugins`, `TicketPacks`, `Drivers`, `Weather`, … - and is searched *before* the original
folder (`omsi_cfg::content_roots`): whatever a mod puts there is found exactly as if it had
been copied into OMSI 2, and a file of the same name replaces the stock one. The original
installation is never written to. `OMSI_CONTENT=/some/dir` moves the content folder.

Installing a mod: the launcher's **Mods** page opens the system's folder / file picker
(Finder, Explorer, GTK) for a mod folder or a `.zip`, `.7z` or `.rar` archive and sorts it
into place (OMSI-style folders anywhere inside are merged; a lone bus, map, object or
spline folder is recognised by its `.bus` / `global.cfg` / `.sco` / `.sli` files and put
under the right folder), or drop it into `Mods/` next to the binary and open the page.
`openomsi-launcher --cli install '{"path":"/path/to/mod.7z"}'` and `--cli mods` do the same
from a shell. An installation is a background job: the archive's table of contents becomes
a plan, the disk is checked for room, everything is unpacked into a staging folder on the
content volume and moved into place in one step, and it can be cancelled and cleaned up at
any point. A repaint for a bus that is not installed is kept aside and installed when the
bus arrives.

Archives can also be **used in place**: a `.zip` laid out like OMSI 2 is put into the
content folder's `Archives/` (hard-linked when it is on the same disk, moved from the
`Mods/` inbox, else copied after a free-space check) and read by the game without
unpacking (`omsi_cfg::vfs` mounts every archive there, as well as `--content-zip` and
`OMSI_CONTENT_ZIP`). The Mods page offers it ("use the archive in place"), and its default
unpacks what fits on the disk and uses an archive in place when its unpacked size does not;
`.7z` and `.rar` archives are always unpacked.
`--cli install '{"path":…,"mode":"inplace"}'` (or `extract` / `auto`) and
`--cli modinfo '{"path":…}'` do the same from a shell. The launcher's lists see the maps
and buses inside the archives.

## Season and weather

The launcher's Departure card has a **Season** choice (spring / summer / autumn / winter,
or by the date as in the original). Choosing one moves the date into that season, so the
timetable and holidays follow, passes `--season` to the game (which picks the map's
seasonal texture folder), and the weather list only offers what fits: snowfall and frost
only in winter, no cold presets in summer.

Weather presets (`Weather/*.owt`) change the light: overcast takes the sun away, rain and
fog thicken the air, a snow preset puts any map into its winter textures with snow cover.

## Radio

OMSI plays no music itself: a bus's radio only sets variables that radio plugins turn into
sound. openOMSI plays internet radio for them, with no plugin needed.

**Which buses.** Every bus whose radio sets `Snd_Radio` (the cassette player of the stock
SD200/SD202/NL and of many mods: it plays the first station) or `SndExt_Radio` (the radios
made for the Sound Extension plugin: station button *n* plays the *n*-th station, the volume
knob `SndVol_Radio` sets how loud). Switch the radio on in the cockpit as in OMSI; the
screen says which station plays and the song when the stream names it.

**Your stations.** `~/.openomsi/radio.cfg` (on Windows `C:\Users\<you>\.openomsi\radio.cfg`)
is written with a few stations the first time the game starts. One station a line, the
first line on the first button:

```
volume = 0.7
Radiozurnal = https://rozhlas.stream/radiozurnal.mp3
Evropa 2 = https://ice.actve.net/fm-evropa2-128
My playlist = https://example.org/station.m3u
```

A station is an MP3, AAC or Ogg stream, or an `.m3u` / `.pls` playlist that points to one.
The address is the one a media player opens - on the station's website, or in a directory
such as radio-browser.info. Streams in HE-AAC with a program config element (some `.aacp`
stations) cannot be played. `volume` (0..1) is the radio's loudness on top of the knob.
The file is read when the game starts.

**Stations of a radio plugin.** Stations already set up for an OMSI radio plugin (SuperRadio
and the like) are taken over: every line with an http(s) address in the text files under
`plugins` is a station, after those of `radio.cfg`.

**Shift+R** moves the whole list one station on, so that the buttons reach the stations
behind the first ones.

**A map's stations.** A map may bring a `radio.cfg` of its own beside its `global.cfg`: its
stations come first on the buttons while you drive on that map, yours after them. It may
also name the frequency each station is on at places of the map - see
[Modding: Radio](MODDING.md#radio-a-maps-stations-and-a-buss-display).

**The radio's display.** A radio with a text display shows the station and the song,
running through its line where they do not fit (ten characters on Dmitrij's "Magnitola" of
P3ta's SOR buses and its kin, whose first line then shows the map's frequency for the
place, e.g. `94.6 MHz`). Without stations, or with the radio off, a display shows its own texts.

**When nothing plays.** `~/.openomsi/game.log` says what happened:
`radio: 23 stations` (the list), `radio: station 1 Radiozurnal (https://…)` (a button
pressed), then `Radio 1: Radiozurnal - buffering …` and the song, or `no signal (…)` with
the reason - mostly an address that is not a stream or a station that is down. No `radio:
station` line at all means the bus's radio sets neither variable.

## Performance

`OMSI_PROFILE=1 … --exit-after N` prints the frame split (render, mirrors, traffic, people,
scripted objects, LAN), counts frames over 50 ms and logs the GPU and CPU memory by kind
every ten seconds; the per-draw buffers are updated in contiguous runs and appended to as
cars and people spawn (no full rebuild), everything behind the fog is culled, culling runs
on all cores, the main pass is recorded as render bundles on helper threads, and the AI
scripts run in parallel. Spandau with traffic, passengers and a storm: 14 → 69 fps on an
M4, no frame over 50 ms after start-up.

Big maps are kept within memory by compressed textures, a texture budget
(`texture_memory`), a timetable fleet read ahead and trimmed again, and tiles that give
everything back when they unload: Ahlheim V5 at its main station with traffic, passengers
and the timetable peaks at 1.93 GB instead of 8.75 GB (see `docs/ARCHITECTURE.md`,
*Memory*). The game's log is `~/.openomsi/game.log` (the launcher's `launcher.log`
beside it), and the first line of both is the build they were made from.

## Object editor

A small part of what OMSI's map editor does, inside the game: **Ctrl+Shift+E** (or *Object
editor* in the game menu) turns it on. **Enter** picks the scenery object nearest the middle
of the view (a magenta glow marks it), **Tab** the next nearest; **I/K/J/L** move it forward,
back, left and right as the camera faces, **U/O** lower and raise it, **N/M** turn it (half a
metre and five degrees a press, a tenth with Shift); **Delete** deletes it (again: back),
**Backspace** undoes everything done to it, **Ctrl+S** saves and **Esc** leaves the editor.
Saving writes each changed tile as a copy into the content folder's map folder
(`<content>/maps/<map>/tile_x_y.map`), which the game reads before the installation - the
original map is never written; delete the copy to have the original back. Only a tile's own
`[object]` records can be edited: splines, the ground, spline rows, new objects and the
timetable are not part of it.

## Mirror panels

Copies of the bus's mirrors can be laid over the picture, so that the street behind is in
view without looking at the glass. In the cab **Ctrl+M** shows or hides them (the first time a
panel appears for each bus); **Ctrl+Shift+M** starts and ends their editor. The panels are only
pictures until the editor is on, so the mouse and the keys work as always. In the editor each
panel has a yellow frame, and:

* the left button drags a panel, the wheel over it resizes it and **Shift+wheel** makes it wider
  or narrower;
* the arrows turn the mirror of the panel under the cursor (as Ctrl+Alt+arrows turns the one
  the driver looks at), **Alt+arrows** shift it across and up and **Page Up/Down** forward and
  back, **-** and **+** narrow and widen its field of view; **R** puts that mirror back as the
  bus has it and **Shift+R** every mirror (turns, shifts and fields of view are kept per bus in
  `mirrors.cfg`);
* **Insert** adds a panel (the main side mirrors first, then the others the bus has), **Delete**
  takes the one under the cursor away and **C** shows another mirror in it;
* **Esc** (or Ctrl+Shift+M again) ends the editor and keeps the layout.

A new panel has the shape of the mirror's glass in the model. The layout is kept per bus in
`~/.openomsi/mirror_hud.cfg`. The setting `mirror_hud` (0 off, 1 the right mirror, 2 the left,
3 both) gives a bus with no layout of its own its first panels. The panels need the mirrors
themselves to be drawn (`mirror_size` not 0); they are redrawn at the rate `mirror_refresh`
sets, also when the glass is not in the view.

## Debug and test switches

Environment variables, all off unless set. The useful ones:

| Variable | What it does |
| --- | --- |
| `OMSI_PROFILE=1`, `OMSI_GPU_TIMERS`, `OMSI_DEBUG_DRAWS` | frame split and memory, per-pass GPU times, draw and changed-instance counts |
| `OMSI_MIRROR_HUD=n` | offscreen: lay the mirror panels (`mirror_hud` 1..3) over the picture |
| `OMSI_SEED=n` | repeat a session: the scripts' `random` is seeded per session (the log says which seed) |
| `OMSI_INPUT="t=3 move x,y; t=3.2 press; t=4 key F3; …"` | drive the real window handlers (mouse, keys, `look`/`turn`) from a script |
| `OMSI_CHURN=x,y` | offscreen check of tile streaming: load the tiles around that far point, unload the start area, unload the far tiles and load the start area again, so the picture is drawn from recycled GPU slots |
| `OMSI_HIDE_WINDOW=from,to` | pretend the window is hidden for those seconds |
| `OMSI_TEXTURE_MEMORY=MB`, `OMSI_BUDGET_FROM=x,y[,MB]` | the texture budget, and meeting it from somewhere else first |
| `OMSI_FLEET_IDLE=s`, `OMSI_FLEET_AHEAD=min` | how long an unused vehicle set is kept, how far ahead the fleet is read |
| `OMSI_NO_BC=1`, `OMSI_NO_TEXCOMPRESS=1`, `OMSI_KEEP_ALLOCATOR=1` | textures as RGBA, no compression of loose pictures, no allocator restart |
| `OMSI_NO_SHADOWS`, `OMSI_NO_CORONAS`, `OMSI_NO_ENVMAP`, `OMSI_NO_BUMP`, `OMSI_NO_CULL`, `OMSI_ENV_PHOTO=0` | leave one part of the picture out for an A/B |
| `OMSI_NO_SURF=1` | roads without the bumps of their textures' `.surf` maps (A/B) |
| `OMSI_NO_PUDDLE_REFLECTIONS=1` | leave wet-road scene reflections out for a screenshot or performance comparison |
| `OMSI_DEBUG_ENHANCED`, `OMSI_DEBUG_SKY`, `OMSI_DEBUG_EXPOSURE`, `OMSI_METER=…` | the enhanced renderer's lamps, sky, adaptation and metering |
| `OMSI_DEBUG_TRAFFIC`, `OMSI_DEBUG_PAX`, `OMSI_DEBUG_PHYSICS`, `OMSI_DEBUG_LAN`, `OMSI_DEBUG_IBIS`, `OMSI_DEBUG_VARS=a,b` | why a car, a passenger, a wheel, a peer, an IBIS or a script variable does what it does |
| `OMSI_CHECK_ROADS=1`, `OMSI_ROAD_PHOTO=1`, `OMSI_CHECK_ENTRIES=1` | walk the lanes as a bus wheel, photograph the carriageway from above, check every entry point |
| `OMSI_CONTENT=/dir`, `OMSI_CONTENT_ZIP=a.zip:b.zip`, `OMSI_ROOT=/dir` | where the content folder, the archives and the original installation are |
| `OMSI_DEBUG_CONES`, `OMSI_NO_LIGHT_MAP`, `OMSI_DEBUG_LIGHT_GRID` | the lamps' fog cones and halos, the tiles' night light maps left out, lights a full grid cell leaves out |
| `OMSI_PARKED_PULL_OUT=p` | the chance per population pass (about 2 s) that a parked car drives off (0.035 by default), with a log of why one does not |
| `OMSI_NO_BRIDGE=1` | a LAN host leaves the internet alone (no UPnP port forward, no address posting) - for tests |
| `OMSI_NO_LAN_MODS=1` | a LAN host serves no mods and a joining game fetches none |
| `OMSI_BACKEND=vulkan\|dx12\|gl` | the graphics interface to ask first (the log lists every adapter each one offers) |
| `OMSI_GPU_LIMITS=default\|downlevel` | pretend the graphics card can only do this much (tests of old cards) |
| `OMSI_GPU_ARRAYS=textures\|nostorage` | read the scene's arrays from textures, as on OpenGL chips without storage buffers in the vertex shader (or without any: no per-pixel lamp light) - tests of old cards |
| `OMSI_RENDER_OCCLUDED=1` | draw even while the window is hidden (tests) |
| `OMSI_CHECK_OBSTACLES=1` | offscreen: drive every lane as a bus and list the objects that would stop it |
| `OMSI_DEBUG_REPEATERS=1` | list the spline object rows whose start the map and the spline chain disagree about |
| `OMSI_TRACE_STEER=file.csv` | write mouse steering, frame by frame (cursor, target, wheel, speed) |

`OMSI_MUTE`, `OMSI_LAN_AUDIO` and `OMSI_LAN_SAY` are the LAN tests' switches; the rest are
listed where they are read (`grep -r OMSI_ crates`).

## LAN play

`--lan-host [port]` hosts a session (UDP, port 27015 by default), `--lan-join <where>`
joins one and `--lan-name` is your name. The launcher's Drive page has the same as *LAN
play: host / join*.

A host prints a **session code** - `OMSI-7Q4K-2M9X-HD3P-R8TZ-KC5W-NB6E`, a base-32
alphabet without look-alike characters, the scrambled session id first and the host's
addresses hidden under a mask drawn from it, so two codes of one host share nothing. The
code carries up to three addresses of the host, a VPN's first: Hamachi (25.x.x.x), Radmin
VPN (26.x.x.x), ZeroTier, Tailscale (100.64.0.0/10), then the LAN's - never the loopback,
a 169.254 address or a bridge of virtual machines. The joining game says hello to all of
them at once and takes the one that answers; with no answer within 10 s it gives up and
says why that may be (not the same network, a firewall). A host that is busy (a dedicated
server loading its map, a game loading a heavy part of one) answers from a thread of its own
while it loads, and the joining game waits up to 8 s for it before it loads anything. The
joining game plays on the host's map whatever map was chosen before joining, when that map is
installed (or comes with the host's mods, below); one that is not is said in the HUD. The
launcher's Sessions page and
the HUD list the same addresses with the network they belong to, for joining by hand.
`--lan-join` takes that code, an `ip`, `ip:port`, a host name, a bare port (a host on this
machine) or `auto` (find a host on the local network by broadcast). Over the internet both
players need the same VPN network; the host's firewall must let the game receive UDP on
port 27015 (Windows counts a Hamachi network as public).

Everybody sends the state of their own bus up to twenty times a second (five while nothing
changes, about 60 bytes: a bit-packed datagram with the pose, pedals, lights, indicators,
doors, wheel travel, the rear sections of an articulated bus and the vehicle's own lamp,
switch and sound variables); everything else is text. The host relays and owns the world:
a joining player takes its date, time, weather and season, and its clock keeps everybody
in step. The other players' buses run their own AI scripts with the sender's inputs, are
drawn and heard where they stand, and are obstacles for the AI traffic like your own bus -
as long as that bus type is installed locally, otherwise your own type stands in for it.
**V** opens the chat line (Enter sends, Esc drops it); joining and leaving are announced
there. The host checks everything it takes in and limits how much a player may send.

**The host's mods.** What the host's session uses that is not in the OMSI 2 folder itself
(its map, bus, objects, splines, AI vehicles and people from the content folder or archives,
also a content folder inside the OMSI 2 folder) is listed with a SHA-256 per file and served
over TCP on the session's port; a joining game fetches what it lacks before it loads the map
and keeps the downloads for the next time (`~/.openomsi/lan-store`). Listing a big add-on map
takes the host a while after it starts (Novi Sad, 27 000 files: 20 s on a fast computer); a joining game waits
for it. Maps installed straight into the OMSI 2 folder are not passed on: both players need
them.

**One world.** The host simulates the AI traffic, the timetable buses, the people on the
pavements and at the stops, the riders of the timetable buses and the traffic lights for
everybody, around every player (it loads the ground and fills the streets around the
others too). A client simulates none of that: it draws the host's world, 160 ms in the
past so that it glides between the host's frames (`crates/omsi-app/src/lan_world.rs`,
`crates/omsi-net/src/world.rs`). Only its own bus and the passengers who board it are its
own: when its bus stands at a stop with a door open, it asks the host for the people
waiting there; the host hands over those still waiting and keeps those who meanwhile went
for another bus, so nobody is ever on two buses. The people walking up to a client's bus
and those getting off are sent back up, so the host sees them too. Money and the timetable
of your own duty stay local - but the host's timetable leaves the tour each player drives to
that player (its AI bus goes, and comes back from the next departure when the player leaves),
and everybody sees the passengers in everybody's bus: the riders of a player's bus travel
with the world frames and sit in that bus in every other game (protocol 5). `crates/omsi-net` is the transport, the same on every
platform.

Windows, macOS and Linux run the same code (wgpu, winit, cpal, std UDP);
paths are resolved case-insensitively so Windows-style `\` references in mods work
everywhere; settings live under `$HOME` / `%USERPROFILE%`. The same `cargo build --release`
produces `openomsi.exe` / `openomsi`, and the scripts in `scripts/` build the launcher tools alongside it. Stopping a game from the launcher uses `WM_CLOSE` on Windows where it
sends SIGTERM elsewhere.
