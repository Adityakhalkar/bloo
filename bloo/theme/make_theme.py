"""Generates assets/themes/bloo/bloo.json from Zed's One theme.

Syntax colors stay as One's (well tuned for contrast). Neutral greys are moved onto a navy hue
taken from the Bloo feather logo, and One's blue accents become Bloo cobalt.

Run from the repo root:  python3 bloo/theme/make_theme.py
"""

import colorsys
import json
import pathlib

SOURCE = pathlib.Path("assets/themes/one/one.json")
TARGET = pathlib.Path("assets/themes/bloo/bloo.json")

# Hue (0-1) of the logo's cobalt, and how much of it neutrals carry.
NAVY_HUE = 224 / 360
VARIANTS = {
    "One Dark": {
        "name": "Bloo Dark",
        "neutral_saturation": 0.30,
        # Darken chrome slightly so the editor reads as deep navy rather than slate.
        "lightness_scale": 0.82,
        "accent": "#6f9bff",
    },
    "One Light": {
        "name": "Bloo Light",
        "neutral_saturation": 0.32,
        "lightness_scale": 1.0,
        "accent": "#1d4ed8",
    },
}


def parse(color: str):
    color = color.lstrip("#")
    red, green, blue = (int(color[i : i + 2], 16) / 255 for i in (0, 2, 4))
    alpha = color[6:8] if len(color) == 8 else "ff"
    return red, green, blue, alpha


def to_hex(red: float, green: float, blue: float, alpha: str) -> str:
    channels = (round(max(0.0, min(1.0, value)) * 255) for value in (red, green, blue))
    return "#" + "".join(f"{value:02x}" for value in channels) + alpha


def is_blue_accent(hue: float, saturation: float) -> bool:
    return 0.53 <= hue <= 0.66 and saturation >= 0.35


def transform(color: str, variant: dict) -> str:
    red, green, blue, alpha = parse(color)
    hue, lightness, saturation = colorsys.rgb_to_hls(red, green, blue)

    if saturation < 0.22:
        # Only darken surfaces; text and icons keep One's lightness so contrast doesn't drop.
        if lightness < 0.5:
            lightness = lightness * variant["lightness_scale"]
        red, green, blue = colorsys.hls_to_rgb(NAVY_HUE, lightness, variant["neutral_saturation"])
        return to_hex(red, green, blue, alpha)

    if is_blue_accent(hue, saturation):
        accent_red, accent_green, accent_blue, _ = parse(variant["accent"])
        accent_hue, accent_lightness, accent_saturation = colorsys.rgb_to_hls(
            accent_red, accent_green, accent_blue
        )
        # Keep One's relative lightness (hover/selected steps) around the new accent.
        lightness = min(0.9, max(0.15, lightness + (accent_lightness - 0.6) * 0.5))
        # Blend saturation so darker accent steps (focus rings, selections) don't turn neon.
        blended_saturation = (saturation + accent_saturation) / 2
        red, green, blue = colorsys.hls_to_rgb(accent_hue, lightness, blended_saturation)
        return to_hex(red, green, blue, alpha)

    return color


def transform_tree(value, variant: dict, in_syntax: bool = False):
    if isinstance(value, dict):
        return {
            key: transform_tree(child, variant, in_syntax or key == "syntax")
            for key, child in value.items()
        }
    if isinstance(value, list):
        return [transform_tree(child, variant, in_syntax) for child in value]
    if isinstance(value, str) and value.startswith("#") and len(value) in (7, 9):
        # Syntax keeps One's colors except its blues, which become Bloo cobalt.
        if in_syntax:
            red, green, blue, _ = parse(value)
            hue, _, saturation = colorsys.rgb_to_hls(red, green, blue)
            return transform(value, variant) if is_blue_accent(hue, saturation) else value
        return transform(value, variant)
    return value


def main() -> None:
    source = json.loads(SOURCE.read_text())
    themes = []
    for theme in source["themes"]:
        variant = VARIANTS[theme["name"]]
        themes.append(
            {
                "name": variant["name"],
                "appearance": theme["appearance"],
                "style": transform_tree(theme["style"], variant),
            }
        )
    family = {
        "$schema": source["$schema"],
        "name": "Bloo",
        "author": "Bloo (derived from Zed Industries' One theme)",
        "themes": themes,
    }
    TARGET.parent.mkdir(parents=True, exist_ok=True)
    TARGET.write_text(json.dumps(family, indent=2) + "\n")
    print(f"wrote {TARGET}")


if __name__ == "__main__":
    main()
