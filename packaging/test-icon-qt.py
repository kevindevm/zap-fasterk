#!/usr/bin/env python3
"""Render the shipped desktop icon with Qt SVG, using no display server."""
import os
from pathlib import Path
import sys

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
from PyQt5.QtCore import qInstallMessageHandler
from PyQt5.QtGui import QImage, QPainter
from PyQt5.QtSvg import QSvgRenderer
from PyQt5.QtWidgets import QApplication

application = QApplication([])
warnings = []
qInstallMessageHandler(lambda kind, context, message: warnings.append(message))
icon = Path(__file__).parent / "icons" / "zap-fasterk.svg"
renderer = QSvgRenderer(str(icon))
assert renderer.isValid(), "Qt rejects the desktop SVG"
image = QImage(256, 256, QImage.Format_ARGB32)
image.fill(0)
painter = QPainter(image)
renderer.render(painter)
painter.end()
assert not warnings, warnings
pixels = [image.pixel(x, y) for y in range(256) for x in range(256)]
assert sum(pixel >> 24 != 0 for pixel in pixels) > len(pixels) // 4, "Icon renders blank"
assert len(set(pixels)) > 16, "Icon loses its detail"
if len(sys.argv) == 2:
    assert image.save(sys.argv[1]), "Could not save rendered evidence"
print("Qt SVG icon rendered without warnings")
