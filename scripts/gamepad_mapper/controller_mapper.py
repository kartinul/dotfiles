#!/usr/bin/env python3
import hid
import json
import os
import queue
import signal
import statistics
import subprocess
import time
from rich.console import Console, Group
from rich.columns import Columns
from rich.panel import Panel
from rich.table import Table
from rich.live import Live
from rich.prompt import Prompt, Confirm
from pynput.keyboard import Controller, Key, KeyCode, Listener


def load_env_file(path):
    if not os.path.exists(path):
        return
    with open(path, "r") as env_file:
        for line in env_file:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            name, value = line.split("=", 1)
            os.environ.setdefault(name.strip(), value.strip().strip('"').strip("'"))


load_env_file(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".env"))
VID = int(os.environ.get("HID_VENDOR_ID", "0x2563"), 0)
PID = int(os.environ.get("HID_PRODUCT_ID", "0x0575"), 0)
CONFIG_FILE = os.environ.get("PAD_CONFIG_FILE", "controller_config.json")

# Standard Xbox layout we want to map to
XBOX_INPUTS = [
    "A",
    "B",
    "X",
    "Y",
    "LB",
    "RB",
    "LT",
    "RT",
    "Select",
    "Start",
    "L3",
    "R3",
    "DPad_Up",
    "DPad_Down",
    "DPad_Left",
    "DPad_Right",
    "LS_X",
    "LS_Y",
    "RS_X",
    "RS_Y",
]
AXIS_INPUTS = {"LT", "RT", "LS_X", "LS_Y", "RS_X", "RS_Y"}
STICK_DIRECTIONS = {
    "LS_X": (("negative", "Left"), ("positive", "Right")),
    "LS_Y": (("positive", "Up"), ("negative", "Down")),
    "RS_X": (("negative", "Left"), ("positive", "Right")),
    "RS_Y": (("positive", "Up"), ("negative", "Down")),
}

console = Console()
kb = Controller()


class GamepadTool:
    def __init__(self):
        self.dev = None
        self.config = {
            "mapping": {},  # Maps Xbox Input Name -> { "type": "button"/"axis", "byte": int, "bit": int(opt) }
            "keybinds": {},  # Maps Xbox Input Name -> Keyboard character (e.g. "w", "space")
        }
        self.active_game = None
        self.ignored_bits = {}
        self.load_config()

    def load_config(self):
        if os.path.exists(CONFIG_FILE):
            with open(CONFIG_FILE, "r") as f:
                self.config = json.load(f)
        games = self.config.setdefault("games", {})
        if not games:
            games["Default"] = {"keybinds": self.config.get("keybinds", {})}
        self.active_game = self.config.get("active_game") or next(iter(games))
        self.config["keybinds"] = games[self.active_game].setdefault("keybinds", {})
        self.ignored_bits = {
            int(byte): set(bits)
            for byte, bits in self.config.get("ignored_bits", {}).items()
        }

    def save_config(self):
        if self.active_game:
            self.config.setdefault("games", {}).setdefault(self.active_game, {})[
                "keybinds"
            ] = self.config["keybinds"]
            self.config["active_game"] = self.active_game
        with open(CONFIG_FILE, "w") as f:
            json.dump(self.config, f, indent=4)

    def connect(self):
        self.dev = hid.device()
        try:
            self.dev.open(VID, PID)
            self.dev.set_nonblocking(True)
            return True
        except OSError as e:
            console.print(f"[red]Failed to open HID device: {e}[/red]")
            console.print("Check your OS Input Monitoring permissions or udev rules.")
            self.dev = None
            return False

    def close(self):
        if self.dev is not None:
            try:
                self.dev.close()
            finally:
                self.dev = None

    def get_baseline(self, samples=20, delay=0.01):
        """Return a representative idle packet, filtering short-lived noise."""
        baselines = []
        for _ in range(samples):
            data = self.dev.read(64)
            if data:
                baselines.append(data)
            time.sleep(delay)

        if not baselines:
            return None

        width = max(len(packet) for packet in baselines)
        self.ignored_bits = {}
        for byte in range(width):
            values = [packet[byte] for packet in baselines if byte < len(packet)]
            noisy = {
                bit
                for bit in range(8)
                if len({bool(value & (1 << bit)) for value in values}) > 1
            }
            if noisy:
                self.ignored_bits[byte] = noisy
        self.config["ignored_bits"] = {
            str(byte): sorted(bits) for byte, bits in self.ignored_bits.items()
        }
        self.save_config()
        return [
            (
                round(
                    statistics.median(
                        packet[i] for packet in baselines if i < len(packet)
                    )
                )
                if any(i < len(packet) for packet in baselines)
                else 0
            )
            for i in range(width)
        ]

    def mapping_panel(self, raw_data=None):
        table = Table(show_header=True, header_style="bold cyan", expand=True)
        table.add_column("Control")
        for name in XBOX_INPUTS:
            if name not in self.config["mapping"]:
                table.add_row(name)
        if not table.rows:
            table.add_row("All controls mapped")
        return Panel(table, title="Remaining controls", border_style="green")

    def meaningful_change(self, byte, value, baseline):
        if byte >= len(baseline):
            return False
        ignored = self.ignored_bits.get(byte, set())
        ignored_mask = sum(1 << bit for bit in ignored)
        return bool((value ^ baseline[byte]) & ~ignored_mask & 0xFF)

    def raw_panel(self, data, baseline, changed=None):
        table = Table(show_header=True, header_style="bold yellow", expand=True)
        table.add_column("Byte", justify="right")
        table.add_column("Value", justify="right")
        changed = changed or set()
        shown = 0
        for byte, value in enumerate(data):
            if byte in changed:
                table.add_row(str(byte), str(value))
                shown += 1
                if shown >= 6:
                    break
        if not shown:
            table.add_row("-", "none yet")
        return Panel(table, title="Live signal", border_style="yellow")

    def candidate_panel(self, candidates):
        table = Table(show_header=True, header_style="bold cyan", expand=True)
        table.add_column("Key", justify="right")
        table.add_column("Current signal")
        for number, candidate in enumerate(candidates, 1):
            signal = (
                f"byte {candidate['byte']} value {candidate['value']} "
                f"({candidate['type']})"
            )
            table.add_row(f"Key {number}", signal)
        if not candidates:
            table.add_row("-", "move or press the requested control")
        return Panel(table, title="Press 1-9 to choose, 0 to skip", border_style="cyan")

    def live_mapping_view(self, data, baseline, message, changed=None, candidates=None):
        return Columns(
            [
                Group(
                    Panel(message, title="Mapping", border_style="cyan"),
                    self.raw_panel(data, baseline, changed),
                    self.candidate_panel(candidates or []),
                ),
                self.mapping_panel(data),
            ],
            equal=True,
            expand=True,
        )

    def candidate_signals(self, frames, baseline):
        if not frames:
            return []
        recent = frames[-6:]
        latest = recent[-1]
        signals = []
        for byte in range(min(len(baseline), len(latest))):
            differing = [
                self.meaningful_change(byte, frame[byte], baseline)
                for frame in recent
                if byte < len(frame)
            ]
            if sum(differing) < min(2, len(recent)):
                continue
            values = [frame[byte] for frame in recent if byte < len(frame)]
            span = max(values) - min(values)
            if span >= 8:
                signals.append(
                    (span + 2, {"type": "axis", "byte": byte, "value": latest[byte]})
                )
                continue
            bit_scores = []
            for bit in range(8):
                if bit in self.ignored_bits.get(byte, set()):
                    continue
                score = sum(
                    bool((frame[byte] ^ baseline[byte]) & (1 << bit))
                    for frame in recent
                    if byte < len(frame)
                )
                if score >= min(2, len(recent)):
                    bit_scores.append((score, bit))
            if bit_scores:
                score, bit = max(bit_scores)
                signals.append(
                    (
                        score,
                        {
                            "type": "button",
                            "byte": byte,
                            "bit": bit,
                            "value": latest[byte],
                        },
                    )
                )
        signals.sort(key=lambda item: item[0], reverse=True)
        return [candidate for _, candidate in signals[:9]]

    def wait_for_input(self, baseline):
        """Keep the dashboard live while the user chooses a numbered signal."""
        frames = []
        changed = set()
        candidates = []
        candidate_keys = set()
        selected = queue.Queue()

        def on_press(key):
            if isinstance(key, KeyCode) and key.char in "1234567890qQ":
                selected.put(key.char.lower())

        try:
            with Listener(on_press=on_press) as listener:
                with Live(
                    self.live_mapping_view(
                        baseline, baseline, "Press or move the requested control."
                    ),
                    refresh_per_second=20,
                    screen=True,
                ) as live:
                    while True:
                        data = self.dev.read(64)
                        if data:
                            frames.append(data)
                            if len(frames) > 20:
                                frames.pop(0)
                            for candidate in self.candidate_signals(frames, baseline):
                                candidate_key = (
                                    candidate["type"],
                                    candidate["byte"],
                                    candidate.get("bit"),
                                )
                                existing = next(
                                    (
                                        item
                                        for item in candidates
                                        if (
                                            item["type"],
                                            item["byte"],
                                            item.get("bit"),
                                        )
                                        == candidate_key
                                    ),
                                    None,
                                )
                                if existing is not None:
                                    existing["value"] = candidate["value"]
                                elif (
                                    candidate_key not in candidate_keys
                                    and len(candidates) < 9
                                ):
                                    candidates.append(candidate)
                                    candidate_keys.add(candidate_key)
                            for i, value in enumerate(data):
                                if self.meaningful_change(i, value, baseline):
                                    changed.add(i)
                            live.update(
                                self.live_mapping_view(
                                    data,
                                    baseline,
                                    "Hold it, then press the number beside the signal.",
                                    changed,
                                    candidates,
                                )
                            )
                            try:
                                key = selected.get_nowait()
                            except queue.Empty:
                                key = None
                            if key == "q":
                                return None
                            if key == "0":
                                return None
                            if key and key.isdigit():
                                number = int(key)
                                if 1 <= number <= len(candidates):
                                    return candidates[number - 1]
                        time.sleep(0.01)
                listener.stop()
        except KeyboardInterrupt:
            return None

    def wait_for_release(self, baseline):
        """Wait until the held control is released, then establish a fresh baseline."""
        with Live(
            self.live_mapping_view(baseline, baseline, "Release the control..."),
            refresh_per_second=20,
        ) as live:
            quiet = 0
            latest = baseline
            while quiet < 5:
                data = self.dev.read(64)
                if data:
                    latest = data
                    is_quiet = all(
                        i >= len(data)
                        or i >= len(baseline)
                        or abs(data[i] - baseline[i]) <= 2
                        for i in range(len(baseline))
                    )
                    quiet = quiet + 1 if is_quiet else 0
                    live.update(
                        self.live_mapping_view(data, baseline, "Release the control...")
                    )
                time.sleep(0.01)
        return self.get_baseline(samples=8, delay=0.01) or latest

    def configure_axis(self, mapping):
        if mapping.get("type") != "axis":
            return mapping
        inverted = Confirm.ask(
            "Invert this axis?",
            default=mapping.get("inverted", False),
        )
        mapping["inverted"] = inverted
        return mapping

    def map_one_control(self, input_name, baseline):
        if input_name in {"LT", "RT"}:
            instruction = f"Pull [bold white]{input_name}[/bold white] fully to 100%."
        elif input_name in AXIS_INPUTS:
            instruction = (
                f"Move [bold white]{input_name}[/bold white] to 0, then move it "
                "to maximum."
            )
        else:
            instruction = f"Press and hold [bold white]{input_name}[/bold white]."
        console.print(f"\n[bold yellow]Action Required:[/bold yellow] {instruction}")
        mapping = self.wait_for_input(baseline)
        if mapping:
            mapping = self.configure_axis(mapping)
            self.config["mapping"][input_name] = mapping
            self.save_config()
            return self.wait_for_release(baseline)
        return baseline

    def wizard_map(self):
        console.clear()
        console.print(
            Panel.fit(
                "[bold magenta]🎮 Gamepad Mapping Wizard[/bold magenta]\nDon't touch the controller while we establish a baseline..."
            )
        )
        time.sleep(1)
        baseline = self.get_baseline()
        if not baseline:
            console.print("[red]No data received from controller.[/red]")
            time.sleep(2)
            return

        console.print("[green]Baseline established.[/green]\n")

        for input_name in XBOX_INPUTS:
            previous = self.config["mapping"].get(input_name)
            baseline_before = baseline
            baseline = self.map_one_control(input_name, baseline)
            if baseline != baseline_before:
                console.print(
                    f"[green]Registered {input_name} -> "
                    f"{self.config['mapping'][input_name]}[/green]"
                )
                console.clear()
            elif not previous:
                console.print(f"[dim]Skipped {input_name}[/dim]")

    def edit_mapping(self):
        if not self.config["mapping"]:
            console.print("[yellow]No mappings to edit yet.[/yellow]")
            time.sleep(1)
            return

        names = list(self.config["mapping"])
        console.clear()
        console.print(Panel.fit("[bold magenta]Edit Controller Mapping[/bold magenta]"))
        for number, name in enumerate(names, 1):
            mapping = self.config["mapping"][name]
            direction = " inverted" if mapping.get("inverted") else ""
            console.print(
                f"{number}. {name} -> byte {mapping['byte']} "
                f"({mapping['type']}{direction})"
            )
        console.print("0. Cancel")
        choice = Prompt.ask(
            "Choose a control to remap",
            choices=[str(number) for number in range(len(names) + 1)],
            default="0",
        )
        if choice == "0":
            return

        input_name = names[int(choice) - 1]
        baseline = self.get_baseline()
        if not baseline:
            console.print("[red]No data received from controller.[/red]")
            time.sleep(1)
            return
        self.map_one_control(input_name, baseline)
        console.print(f"[green]Updated {input_name}.[/green]")
        time.sleep(1)

    def parse_state(self, data):
        """Converts raw 64-byte array into logical Xbox state based on mapping."""
        state = {}
        for name, m in self.config["mapping"].items():
            if m["byte"] >= len(data):
                continue

            val = data[m["byte"]]
            if m["type"] == "button":
                is_pressed = bool(val & (1 << m["bit"]))
                state[name] = 1 if is_pressed else 0
            elif m["type"] == "axis":
                # Normalize axis from -1.0 to 1.0 (or 0.0 to 1.0 for triggers)
                mid = 127.5
                if name in ["LT", "RT"]:
                    norm = val / 255.0
                    state[name] = max(0.0, min(1.0, norm))
                else:
                    norm = (val - mid) / 127.5
                    dz = 0.08
                    if abs(norm) < dz:
                        norm = 0.0
                    if m.get("inverted", False):
                        norm = -norm
                    state[name] = max(-1.0, min(1.0, norm))
        return state

    def test_controller(self):
        if not self.config["mapping"]:
            console.print("[red]No mapping found. Run the mapping wizard first.[/red]")
            time.sleep(2)
            return

        def generate_table(state):
            table = Table(
                title="Controller Live Test (Ctrl+C to exit)", title_style="bold cyan"
            )
            table.add_column("Input", style="magenta")
            table.add_column("State", style="green")

            for k, v in state.items():
                if isinstance(v, float):
                    bar = "█" * int(abs(v) * 20)
                    dir_char = "-" if v < 0 else "+"
                    table.add_row(k, f"{v:5.2f} [{dir_char}] {bar}")
                else:
                    table.add_row(
                        k, "[bold red]PRESSED[/bold red]" if v else "Released"
                    )
            return table

        try:
            with Live(generate_table({}), refresh_per_second=30) as live:
                while True:
                    data = self.dev.read(64)
                    if data:
                        state = self.parse_state(data)
                        live.update(generate_table(state))
                    time.sleep(0.01)
        except KeyboardInterrupt:
            pass

    def select_game(self):
        games = self.config.setdefault("games", {})
        console.clear()
        console.print(Panel.fit("[bold magenta]Select game profile[/bold magenta]"))
        console.print("n. New game")
        names = list(games)
        for number, name in enumerate(names, 1):
            console.print(f"{number}. {name}")
        choices = ["n"] + [str(number) for number in range(1, len(names) + 1)]
        choice = Prompt.ask("Game", choices=choices, default="1" if names else "n")
        if choice == "n":
            name = Prompt.ask("Enter the full game name").strip()
            if not name:
                name = "Default"
            games.setdefault(name, {"keybinds": {}})
        else:
            name = names[int(choice) - 1]
        self.active_game = name
        self.config["keybinds"] = games[name].setdefault("keybinds", {})
        self.save_config()
        return self.config["keybinds"]

    def map_keyboard(self):
        keybinds = self.select_game()
        console.clear()
        console.print(
            Panel.fit(
                f"[bold magenta]⌨️ Keyboard Binder[/bold magenta]\nGame: {self.active_game}"
            )
        )
        for btn in XBOX_INPUTS:
            if btn in self.config["mapping"]:
                mapping = self.config["mapping"][btn]
                if btn in STICK_DIRECTIONS:
                    directions = {}
                    for direction, label in STICK_DIRECTIONS[btn]:
                        key = self.capture_keyboard_key(f"{btn} {label}")
                        if key:
                            directions[direction] = key
                            console.print(
                                f"[green]Mapped {btn} {label} -> "
                                f"{self.display_key_name(key)}[/green]"
                            )
                    if directions:
                        keybinds[btn] = {
                            "mode": "directions",
                            "directions": directions,
                            "threshold": 0.5,
                        }
                else:
                    key = self.capture_keyboard_key(btn)
                    if key:
                        console.print(
                            f"[green]Mapped {btn} -> {self.display_key_name(key)}[/green]"
                        )
                        if mapping["type"] == "axis":
                            keybinds[btn] = {
                                "key": key,
                                "mode": "button",
                                "direction": "positive",
                                "threshold": 0.5,
                            }
                        else:
                            keybinds[btn] = key
        self.config["keybinds"] = keybinds
        self.save_config()
        console.print("[green]Keybinds saved![/green]")
        time.sleep(1)

    @staticmethod
    def display_key_name(key):
        """Return a readable label for a captured pynput key name."""
        special_names = {
            "space": "Space",
            "enter": "Enter",
            "tab": "Tab",
            "backspace": "Backspace",
            "shift": "Shift",
            "ctrl": "Ctrl",
            "alt": "Alt",
            "cmd": "Command",
            "esc": "Escape",
        }
        return special_names.get(key, key.upper() if len(key) == 1 else key)

    def capture_keyboard_key(self, input_name):
        """Wait for the next keyboard event and return its pynput name."""
        captured = queue.Queue()
        interrupt = object()
        ctrl_down = set()

        def on_press(key):
            if isinstance(key, Key) and key.name.startswith("ctrl"):
                ctrl_down.add(key)
                return
            if (
                isinstance(key, KeyCode)
                and key.char
                and key.char.lower() == "c"
                and ctrl_down
            ):
                captured.put(interrupt)
                return False
            if isinstance(key, KeyCode):
                if key.char:
                    captured.put(key.char)
            else:
                captured.put(key.name)
            return False

        console.print(
            f"Map [cyan]{input_name}[/cyan]: press a key (\\ skips this control)."
        )
        with Listener(on_press=on_press, suppress=True) as listener:
            listener.join()

        key = captured.get()
        if key is interrupt:
            raise KeyboardInterrupt
        return "" if key == "\\" else key

    def run_emulation(self):
        keybinds = self.select_game()
        if not keybinds:
            console.print("[red]No keybinds found. Map keyboard first.[/red]")
            time.sleep(2)
            return

        console.clear()
        console.print(
            Panel.fit(
                "[bold green]🚀 Emulation Running[/bold green]\nPress Ctrl+C to stop."
            )
        )

        last_state = {}
        last_direction_state = {}
        try:
            while True:
                data = self.dev.read(64)
                if data:
                    state = self.parse_state(data)
                    for btn, bind in keybinds.items():
                        if isinstance(bind, dict) and bind.get("mode") == "directions":
                            current = state.get(btn, 0)
                            threshold = float(bind.get("threshold", 0.5))
                            for direction, pynput_binding in bind.get(
                                "directions", {}
                            ).items():
                                if not pynput_binding:
                                    continue
                                active = (
                                    current >= threshold
                                    if direction == "positive"
                                    else current <= -threshold
                                )
                                state_key = (btn, direction)
                                previous = last_direction_state.get(state_key, False)
                                pynput_key = pynput_binding
                                if hasattr(Key, pynput_binding):
                                    pynput_key = getattr(Key, pynput_binding)
                                if active and not previous:
                                    kb.press(pynput_key)
                                elif not active and previous:
                                    kb.release(pynput_key)
                                last_direction_state[state_key] = active
                            continue

                        if isinstance(bind, dict):
                            current = state.get(btn, 0)
                            if bind.get("mode") == "button":
                                threshold = float(bind.get("threshold", 0.5))
                                if bind.get("direction", "positive") == "negative":
                                    current = 1 if current <= -threshold else 0
                                else:
                                    current = 1 if current >= threshold else 0
                            pynput_binding = bind.get("key", "")
                        else:
                            current = state.get(btn, 0)
                            pynput_binding = bind
                        previous = last_state.get(btn, 0)

                        # Handle standard keys vs special keys
                        pynput_key = pynput_binding
                        if hasattr(Key, pynput_binding):
                            pynput_key = getattr(Key, pynput_binding)

                        # Button press/release logic
                        if current > 0.5 and previous <= 0.5:
                            kb.press(pynput_key)
                        elif current <= 0.5 and previous > 0.5:
                            kb.release(pynput_key)

                    last_state = state
                time.sleep(0.01)
        except KeyboardInterrupt:
            # Release all keys on exit
            for bind in keybinds.values():
                if isinstance(bind, dict) and bind.get("mode") == "directions":
                    bindings = bind.get("directions", {}).values()
                else:
                    bindings = [bind.get("key", "") if isinstance(bind, dict) else bind]
                for pynput_binding in bindings:
                    if not pynput_binding:
                        continue
                    if hasattr(Key, pynput_binding):
                        kb.release(getattr(Key, pynput_binding))
                    else:
                        kb.release(pynput_binding)
            pass

    def run_native_runtime(self):
        """Build and launch the low-latency Swift runtime."""
        base_dir = os.path.dirname(os.path.abspath(__file__))
        source_path = os.path.join(base_dir, "keyboard_runtime.swift")
        binary_path = os.path.join(base_dir, "keyboard_runtime")

        if not os.path.exists(source_path):
            console.print(f"[red]Missing Swift runtime: {source_path}[/red]")
            return

        needs_build = not os.path.exists(binary_path) or os.path.getmtime(
            source_path
        ) > os.path.getmtime(binary_path)
        try:
            if needs_build:
                console.print("[cyan]Building native runtime...[/cyan]")
                subprocess.run(
                    [
                        "swiftc",
                        source_path,
                        "-o",
                        binary_path,
                        "-framework",
                        "IOKit",
                        "-framework",
                        "CoreGraphics",
                    ],
                    cwd=base_dir,
                    check=True,
                )
            console.print("[green]Starting native runtime...[/green]")
            subprocess.run([binary_path], cwd=base_dir, check=False)
        except FileNotFoundError:
            console.print(
                "[red]swiftc was not found. Install Xcode Command Line Tools.[/red]"
            )
        except subprocess.CalledProcessError as error:
            console.print(
                f"[red]Swift build failed with exit code {error.returncode}.[/red]"
            )

    def menu(self):
        while True:
            console.clear()
            console.print(
                Panel.fit(
                    "[bold cyan]🎮 Raw HID Master[/bold cyan]\nBy kartik",
                    border_style="cyan",
                )
            )

            if not self.config["mapping"]:
                console.print(
                    "[yellow]⚠️  No mapping found. Please run the mapping wizard first.[/yellow]\n"
                )

            console.print("1. 🎮 Auto-Map Controller to Xbox Layout")
            console.print("2. 🧪 Live Test Controller")
            console.print("3. ✏️  Edit Controller Mapping")
            console.print("4. ⌨️  Map Controller to Keyboard")
            console.print("5. 🚀 Start Keyboard Emulation")
            console.print("6. ❌ Exit\n")

            choice = Prompt.ask(
                "Select an option", choices=["1", "2", "3", "4", "5", "6"]
            )

            if choice == "1":
                self.wizard_map()
            elif choice == "2":
                self.test_controller()
            elif choice == "3":
                self.edit_mapping()
            elif choice == "4":
                self.map_keyboard()
            elif choice == "5":
                self.select_game()
                self.close()
                self.run_native_runtime()
                break
            elif choice == "6":
                break


if __name__ == "__main__":

    def stop_process(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGINT, stop_process)
    signal.signal(signal.SIGTSTP, stop_process)
    app = GamepadTool()
    try:
        if app.connect():
            # If no mapping exists on startup, force the wizard first
            if not app.config.get("mapping"):
                if Confirm.ask("No config found. Run the mapping wizard now?"):
                    app.wizard_map()
            app.menu()
    except (KeyboardInterrupt, SystemExit) as error:
        console.print("\n[yellow]Stopped. HID device released.[/yellow]")
        if isinstance(error, SystemExit):
            raise error
    finally:
        app.close()
