#!/usr/bin/env python3
"""
pdf_adjust.py - Change how a PDF looks (brightness, contrast, color, etc.)

How it works: each page is rendered to an image, the effects are applied,
and the images are stitched back into a new PDF.
NOTE: the output is image-based, so text is no longer selectable/searchable.

Install:
    pip install pypdfium2 pillow

Examples:
    python pdf_adjust.py in.pdf out.pdf --brightness 1.2 --contrast 1.3
    python pdf_adjust.py in.pdf out.pdf --grayscale --sharpness 2
    python pdf_adjust.py in.pdf out.pdf --invert                  # dark mode
    python pdf_adjust.py in.pdf out.pdf --sepia --dpi 200
    python pdf_adjust.py in.pdf out.pdf --threshold 140           # scan cleanup (B&W)
    python pdf_adjust.py in.pdf out.pdf --pages 1-3,7 --gamma 0.8
"""

import argparse
import sys

import pypdfium2 as pdfium
from PIL import Image, ImageEnhance, ImageFilter, ImageOps


def parse_pages(spec, total):
    """'1-3,7' -> [0, 1, 2, 6] (zero-based). None -> all pages."""
    if not spec:
        return list(range(total))
    pages = []
    for part in spec.split(","):
        part = part.strip()
        if "-" in part:
            a, b = part.split("-")
            pages.extend(range(int(a) - 1, int(b)))
        else:
            pages.append(int(part) - 1)
    return [p for p in pages if 0 <= p < total]


def apply_gamma(img, gamma):
    inv = 1.0 / gamma
    lut = [round(255 * ((i / 255) ** inv)) for i in range(256)]
    return img.point(lut * len(img.getbands()))


def apply_sepia(img):
    gray = ImageOps.grayscale(img)
    return ImageOps.colorize(gray, black="#2b1a0a", white="#f5e6c8", mid="#a67c52")


def adjust(img, a):
    img = img.convert("RGB")

    if a.gamma != 1.0:
        img = apply_gamma(img, a.gamma)
    if a.brightness != 1.0:
        img = ImageEnhance.Brightness(img).enhance(a.brightness)
    if a.contrast != 1.0:
        img = ImageEnhance.Contrast(img).enhance(a.contrast)
    if a.saturation != 1.0:
        img = ImageEnhance.Color(img).enhance(a.saturation)
    if a.sharpness != 1.0:
        img = ImageEnhance.Sharpness(img).enhance(a.sharpness)
    if a.blur > 0:
        img = img.filter(ImageFilter.GaussianBlur(a.blur))
    if a.autocontrast:
        img = ImageOps.autocontrast(img, cutoff=1)

    if a.grayscale:
        img = ImageOps.grayscale(img).convert("RGB")
    if a.sepia:
        img = apply_sepia(img)
    if a.threshold is not None:
        img = ImageOps.grayscale(img).point(
            lambda p: 255 if p >= a.threshold else 0
        ).convert("RGB")
    if a.invert:
        img = ImageOps.invert(img)

    return img


def main():
    p = argparse.ArgumentParser(description="Adjust the look of a PDF.")
    p.add_argument("input")
    p.add_argument("output")

    p.add_argument("--brightness", type=float, default=1.0, help="1.0 = unchanged, >1 brighter (default 1.0)")
    p.add_argument("--contrast", type=float, default=1.0, help="1.0 = unchanged, >1 more contrast")
    p.add_argument("--saturation", type=float, default=1.0, help="0 = grayscale, 1 = unchanged, >1 more vivid")
    p.add_argument("--sharpness", type=float, default=1.0, help="0 = blurred, 1 = unchanged, >1 sharper")
    p.add_argument("--gamma", type=float, default=1.0, help="<1 darker midtones, >1 lighter midtones")
    p.add_argument("--blur", type=float, default=0.0, help="Gaussian blur radius in px")
    p.add_argument("--autocontrast", action="store_true", help="Stretch levels automatically")
    p.add_argument("--grayscale", action="store_true", help="Convert to black & white shades")
    p.add_argument("--sepia", action="store_true", help="Warm vintage tone")
    p.add_argument("--invert", action="store_true", help="Invert colors (dark mode)")
    p.add_argument("--threshold", type=int, metavar="0-255", help="Pure black/white cutoff (good for scans)")

    p.add_argument("--dpi", type=int, default=150, help="Render resolution (default 150; higher = sharper, bigger file)")
    p.add_argument("--quality", type=int, default=90, help="JPEG quality inside the PDF (default 90)")
    p.add_argument("--pages", help="Pages to process, e.g. '1-3,7' (default: all)")
    args = p.parse_args()

    pdf = pdfium.PdfDocument(args.input)
    indices = parse_pages(args.pages, len(pdf))
    if not indices:
        sys.exit("No valid pages selected.")

    scale = args.dpi / 72
    out_pages = []
    for n, i in enumerate(indices, 1):
        print(f"Processing page {i + 1} ({n}/{len(indices)})...")
        img = pdf[i].render(scale=scale).to_pil()
        out_pages.append(adjust(img, args))

    out_pages[0].save(
        args.output,
        save_all=True,
        append_images=out_pages[1:],
        resolution=args.dpi,
        quality=args.quality,
    )
    print(f"Done -> {args.output}")


if __name__ == "__main__":
    main()