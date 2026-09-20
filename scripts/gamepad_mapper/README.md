# Raw HID Gamepad Mapper

to map any random ahh controller or input source to keyboards ig? axis / butotn based detcion.
it detcts anything.... any damn input given... in BITS BYTES whatver.... needs the id tho

## Requirements

- macOS
- Xcode Command Line Tools (`swiftc`)
- Python 3
- Python packages used by the mapper:

```bash
python3 -m pip install hidapi rich pynput
```

Grant **Input Monitoring** permission to the terminal or editor that runs the program. Grant **Accessibility** permission to that same app so macOS accepts generated keyboard events.

## Environment

Runtime settings are stored in `.env`. For a new setup, copy the template and adjust the values if needed:

```bash
cp .env.example .env
```

The included `.env` is populated for the configured controller. Shell environment variables override values from `.env`.

## Run

From this directory:

```bash
python3 controller_mapper.py
```

Use the mapper options to create or update `controller_config.json`. Choose **Start Keyboard Emulation** when ready. The Python tool closes its HID handle, compiles `keyboard_runtime.swift` when the source is newer than the executable, and starts the native runtime.

You can also run the native runtime directly:

```bash
swiftc keyboard_runtime.swift -o keyboard_runtime -framework IOKit -framework CoreGraphics
./keyboard_runtime
```

Press `Ctrl+C` to stop the native runtime. It releases held keys before exiting.

## Configuration

The mapper writes:

- `mapping`: HID byte and bit locations for controller controls
- `keybinds`: default keyboard bindings
- `games`: per-game keyboard profiles
- `active_game`: profile used by the native runtime

Keep `keyboard_runtime`, `controller_config.json`, and `keyboard_runtime.swift` in the same directory when launching the Swift runtime directly.
