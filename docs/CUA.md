# Driving the app with a computer-use agent

The goal: an agent that *looks* at the running window, *acts* on it, and looks again. On
this Omarchy machine that loop is split across two tools, because no single one covers both
halves here. This page says exactly what works, what does not, and why.

```
look:  tools/cua/server.py   (CUA's computer server, screenshot patched to grim)
act:   tools/cua/drive.py    (keys through wtype → the compositor's virtual keyboard)
loop:  tools/cua/drive.py --self-test
```

## Running it

```sh
# once: a Python just for CUA
uv venv /tmp/cuavenv
VIRTUAL_ENV=/tmp/cuavenv uv pip install cua-computer cua-computer-server

# a display the app can draw on and a camera can see (see "Why the split" below)
hyprctl output create headless
OUT=$(hyprctl monitors -j | python3 -c 'import json,sys;print([m["name"] for m in json.load(sys.stdin) if m["name"].startswith("HEADLESS")][0])')

# the CUA server, with the screenshot patched to grim on that output
OMAD_CUA_OUTPUT=$OUT /tmp/cuavenv/bin/python tools/cua/server.py --host 127.0.0.1 --port 7799 &

# the loop
OMAD_CUA_OUTPUT=$OUT OMAD_BIN=./target/debug/omalldownloader \
  /tmp/cuavenv/bin/python tools/cua/drive.py --self-test
```

The self test opens the app on the headless output, captures it with CUA, sends Tab/Tab/Return
to move to the Add page, tries to type a URL, and then reports what happened. It exits `0`
when the download completed, `2` when the loop looked and acted but nothing was typed.

## What works

* **Capturing the window through CUA.** `POST /cmd {"command":"screenshot"}` returns a real
  frame (≈50 KB PNG, thousands of colours) of the app on the headless output. Verified.
* **Keyboard navigation.** `wtype -k Tab` / `-k Return` move focus and change the page;
  `docs/CUA.md`'s self test asserts this by comparing frames and by the page it lands on.
  Verified.

## What does not work yet

* **Pointer input.** CUA's Linux driver clicks with `pynput`, which injects through XTEST.
  Under this session's Xwayland, XTEST events are not delivered — a plain Xlib `XTEST`
  click aimed at a plain X11 window created for the purpose also never arrived, so this is
  the Xwayland/XTest path, not GPUI.
* **`ydotool` absolute pointer moves.** `ydotool mousemove -a -x 2017 -y 99` reports success
  but `hyprctl cursorpos` stays where it was. **Relative** moves do work
  (`-x -500` moved 2880 → 1954 → 1199), so `drive.py` walks the pointer to a target in
  relative steps.
* **Clicks and typed text in the field.** The pointer can be positioned, but neither a
  `ydotool click 0xC0` nor `wtype -- "<text>"` produces a visible effect in the text field:
  focus does not appear and no characters arrive. Keys for navigation (Tab, Return) do work,
  which is why the loop can open the Add page but cannot fill it.

## Why the split, and why the X11 detour

* CUA's Linux screenshot is `PIL.ImageGrab`, which reads the X root window. Under Xwayland a
  GL client's pixels live in a Wayland surface, so every capture was **black**. `server.py`
  patches that one method to `grim -o <output>`, which sees the compositor's real frame.
* CUA's input is X11-only, so the app must be an X11 client for it to have any chance:
  `DISPLAY=:0` (the session's Xwayland) with `WAYLAND_DISPLAY` unset makes GPUI take its X11
  backend. GPUI cannot use Xvfb — it fails with `Failed to create surface: PlatformNotSupported`
  because there is no DRI3 — so Xvfb is not an option, and Xwayland is the only X server here
  that GPUI accepts.

## The three fixes that would finish it

1. **Find out why injected buttons and characters do not land.** Clicks and text are the
   remaining gap; the pointer moves, so the virtual devices exist. Next steps: run
   `ydotool debug` (it prints the events it sends), check `libinput debug-events` while a
   synthetic click is sent, and confirm the compositor is not dropping synthetic input for
   security.
2. **Make CUA's Linux handler use `wtype`/`ydotool` instead of pynput** for input — a patch
   like the screenshot one, but for `left_click`, `mouse_down`, `type_text` and `key`.
3. **Report the XTEST gap upstream**: CUA assumes a plain Xorg desktop.

Until one of those lands, the honest description is: *CUA can see the application; the
compositor can drive it by keyboard; the two have not been joined into one pointer-capable
loop on this machine.*
