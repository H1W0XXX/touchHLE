"""Launch touchHLE, close Zombie Farm's startup bar, then click the screen center.

This is intentionally small and Windows-focused because it is for local
regression loops while bringing Zombie Farm up.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from pathlib import Path

import cv2
import numpy as np
import pyautogui
import win32gui


def find_touchhle_window(title_hint: str, timeout: float) -> int:
    deadline = time.time() + timeout
    while time.time() < deadline:
        matches: list[int] = []

        def enum_window(hwnd: int, _: object) -> None:
            if not win32gui.IsWindowVisible(hwnd):
                return
            title = win32gui.GetWindowText(hwnd)
            if "touchHLE" in title and title_hint.lower() in title.lower():
                matches.append(hwnd)

        win32gui.EnumWindows(enum_window, None)
        if matches:
            return matches[0]
        time.sleep(0.1)
    raise RuntimeError(f"Could not find a touchHLE window matching {title_hint!r}")


def client_rect_on_screen(hwnd: int) -> tuple[int, int, int, int]:
    left, top = win32gui.ClientToScreen(hwnd, (0, 0))
    right, bottom = win32gui.ClientToScreen(hwnd, win32gui.GetClientRect(hwnd)[2:])
    return left, top, right - left, bottom - top


def find_red_close_button(image_rgb: np.ndarray) -> tuple[int, int] | None:
    height, width = image_rgb.shape[:2]
    red = (
        (image_rgb[:, :, 0] > 145)
        & (image_rgb[:, :, 1] < 95)
        & (image_rgb[:, :, 2] < 95)
    ).astype(np.uint8) * 255

    contours, _ = cv2.findContours(red, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    candidates: list[tuple[int, int, int, int, int]] = []
    for contour in contours:
        x, y, w, h = cv2.boundingRect(contour)
        area = cv2.contourArea(contour)
        if area < 30 or w < 8 or h < 8:
            continue
        if x < width * 0.80:
            continue
        if y < height * 0.42 or y > height * 0.70:
            continue

        pad = max(4, int(max(w, h) * 0.25))
        x0, y0 = max(0, x - pad), max(0, y - pad)
        x1, y1 = min(width, x + w + pad), min(height, y + h + pad)
        crop = image_rgb[y0:y1, x0:x1]
        white = (
            (crop[:, :, 0] > 215)
            & (crop[:, :, 1] > 215)
            & (crop[:, :, 2] > 215)
        )
        if int(white.sum()) < 8:
            continue
        candidates.append((int(area), x, y, w, h))

    if not candidates:
        return None
    _, x, y, w, h = max(candidates)
    return x + w // 2, y + h // 2


def screenshot_client(hwnd: int) -> tuple[np.ndarray, tuple[int, int, int, int]]:
    rect = client_rect_on_screen(hwnd)
    pil_image = pyautogui.screenshot(region=rect)
    return np.asarray(pil_image.convert("RGB")), rect


def click_client(hwnd: int, x: int, y: int) -> None:
    left, top, _, _ = client_rect_on_screen(hwnd)
    pyautogui.click(left + x, top + y)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("ipa", nargs="?", default=r"zombie_farm\ZFR06.ipa")
    parser.add_argument("--exe", default=r"target\release\touchHLE.exe")
    parser.add_argument("--log", default="touchhle_auto_click.log")
    parser.add_argument("--title", default="")
    parser.add_argument("--startup-timeout", type=float, default=15.0)
    parser.add_argument("--close-timeout", type=float, default=10.0)
    parser.add_argument("--after-close-delay", type=float, default=2.0)
    parser.add_argument("--after-center-wait", type=float, default=12.0)
    parser.add_argument("--center-ratio-x", type=float, default=0.5)
    parser.add_argument("--center-ratio-y", type=float, default=0.56)
    parser.add_argument("--debug-screenshot", default="")
    parser.add_argument("--attach", action="store_true")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[1]
    exe = (repo / args.exe).resolve()
    ipa = (repo / args.ipa).resolve()
    log_path = (repo / args.log).resolve()

    proc: subprocess.Popen[bytes] | None = None
    if not args.attach:
        log = log_path.open("wb")
        proc = subprocess.Popen(
            [str(exe), str(ipa)],
            cwd=repo,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
    title_hint = args.title or ("ZFR" if "ZFR" in ipa.name else "ZombieFarm")
    hwnd = find_touchhle_window(title_hint, args.startup_timeout)

    close_pos: tuple[int, int] | None = None
    last_image: np.ndarray | None = None
    last_rect: tuple[int, int, int, int] | None = None
    deadline = time.time() + args.close_timeout
    while time.time() < deadline:
        image, rect = screenshot_client(hwnd)
        last_image = image
        last_rect = rect
        close_pos = find_red_close_button(image)
        if close_pos is not None:
            break
        time.sleep(0.2)

    if close_pos is None:
        print("red close button not found", file=sys.stderr)
        if args.debug_screenshot and last_image is not None:
            debug_path = (repo / args.debug_screenshot).resolve()
            cv2.imwrite(str(debug_path), cv2.cvtColor(last_image, cv2.COLOR_RGB2BGR))
            print(f"saved debug screenshot to {debug_path}", file=sys.stderr)
        if last_rect is None:
            _, last_rect = screenshot_client(hwnd)
        _, _, width, height = last_rect
        close_pos = (int(width * 0.92), int(height * 0.54))
        print(f"fallback close click at client {close_pos}", file=sys.stderr)
        click_client(hwnd, *close_pos)
    else:
        print(f"click close at client {close_pos}")
        click_client(hwnd, *close_pos)

    time.sleep(args.after_close_delay)
    _, (_, _, width, height) = screenshot_client(hwnd)
    center = (int(width * args.center_ratio_x), int(height * args.center_ratio_y))
    print(f"click center at client {center}")
    click_client(hwnd, *center)

    if proc is not None:
        try:
            proc.wait(timeout=args.after_center_wait)
        except subprocess.TimeoutExpired:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
