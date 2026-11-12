"""Build fastbrowse's focus cursor using Cua Driver's dotLottie theme format."""

import json
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1] / "crates/fastbrowse-browser/assets/cursor"
ACTIONS = [
    "idle",
    "observe",
    "click",
    "drag",
    "scroll",
    "text",
    "key",
    "navigate",
    "app",
    "transfer",
    "record",
    "system",
]
EMBER = [197 / 255, 82 / 255, 48 / 255, 1]


def fixed(value):
    return {"a": 0, "k": value}


def stroke(color, width):
    return {"ty": "st", "c": fixed(color), "o": fixed(100), "w": fixed(width), "lc": 2, "lj": 2}


def animation(action):
    corners = [
        [(28, 46), (28, 28), (46, 28)],
        [(82, 28), (100, 28), (100, 46)],
        [(100, 82), (100, 100), (82, 100)],
        [(46, 100), (28, 100), (28, 82)],
    ]
    paths = [{"ty": "sh", "ks": fixed({"v": p, "i": [[0, 0]] * 3, "o": [[0, 0]] * 3, "c": False})} for p in corners]
    dot = {"ty": "el", "p": fixed([64, 64]), "s": fixed([9, 9]), "d": 1}
    fill = {"ty": "fl", "c": fixed(EMBER), "o": fixed(100), "r": 1}
    layers = []
    for i, shapes in enumerate(([dot, fill], [*paths, stroke(EMBER, 7)], [*paths, stroke([1, 1, 1, 1], 13)])):
        layers.append(
            {
                "ddd": 0,
                "ind": i + 1,
                "ty": 4,
                "nm": "Focus",
                "sr": 1,
                "ks": {
                    "o": fixed(100),
                    "r": fixed(0),
                    "p": fixed([64, 64]),
                    "a": fixed([64, 64]),
                    "s": fixed([100, 100]),
                },
                "ao": 0,
                "shapes": shapes,
                "ip": 0,
                "op": 24,
                "st": 0,
                "bm": 0,
            }
        )
    return {
        "v": "5.7.4",
        "fr": 30,
        "ip": 0,
        "op": 24,
        "w": 128,
        "h": 128,
        "nm": action,
        "ddd": 0,
        "assets": [],
        "layers": layers,
    }


def main():
    theme = {
        "schema": "cua.cursor-theme/2",
        "id": "fastbrowse.focus.v1",
        "name": "fastbrowse focus",
        "version": "1.1.0",
        "author": "Agent Labs",
        "license": "MIT",
        "compatibility": {"profile": "cua-driver-actions-v2", "semantics": 2},
        "canvas": {"width": 128, "height": 128, "fps": 30},
        "hotspot": {"x": 64, "y": 64},
        "actions": {a: {"animation": f"action_{a}", "still_frame": 0} for a in ACTIONS},
    }
    entries = {
        "manifest.json": {"version": "2", "animations": [{"id": f"action_{a}"} for a in ACTIONS]},
        "cua/theme.json": theme,
    }
    entries.update({f"a/action_{a}.json": animation(a) for a in ACTIONS})
    with ZipFile(ROOT / "fastbrowse.focus.lottie", "w") as archive:
        for name, value in entries.items():
            info = ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    main()
