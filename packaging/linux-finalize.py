#!/usr/bin/env python3
"""Finalize Linux outputs before native-packages records their checksums."""
import os
from pathlib import Path
import re
import struct
import subprocess
import sys


def check_abi(binary):
    versions = subprocess.check_output(["readelf", "--version-info", str(binary)], text=True)
    required = [tuple(map(int, match)) for match in re.findall(r"GLIBC_(\d+)\.(\d+)", versions)]
    if not required or max(required) > (2, 35):
        raise ValueError("Linux input exceeds the declared glibc 2.35 baseline; rebuild on Ubuntu 22.04")


def update_section(data):
    if data[:6] != b"\x7fELF\x02\x01" or data[8:11] != b"AI\x02":
        raise ValueError("Expected a little-endian ELF64 type-2 AppImage")
    offset = struct.unpack_from("<Q", data, 40)[0]
    entry_size, count, names_index = struct.unpack_from("<HHH", data, 58)
    if entry_size != 64 or names_index >= count or offset + count * entry_size > len(data):
        raise ValueError("Invalid ELF section table")
    names_offset, names_size = struct.unpack_from("<QQ", data, offset + names_index * 64 + 24)
    if names_offset + names_size > len(data):
        raise ValueError("Invalid ELF section names")
    names = data[names_offset:names_offset + names_size]
    for index in range(count):
        entry = offset + index * 64
        name_index = struct.unpack_from("<I", data, entry)[0]
        if name_index >= len(names):
            raise ValueError("Invalid ELF section name")
        if names[name_index:].split(b"\0", 1)[0] == b".upd_info":
            start, size = struct.unpack_from("<QQ", data, entry + 24)
            if start < 64 or start + size > len(data):
                raise ValueError("Invalid AppImage update section")
            return start, size
    raise ValueError("AppImage runtime has no reserved update-information section")


def finalize(package, architecture):
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}[architecture]
    # Update metadata is passive. The app's verified built-in updater is unchanged.
    text = (f"gh-releases-zsync|kevindevm|zap-fasterk|latest|"
            f"zap-fasterk-*-{architecture}.AppImage.zsync").encode("ascii")
    data = package.read_bytes()
    start, size = update_section(data)
    if len(text) >= size:
        raise ValueError("Update information exceeds the runtime's reserved section")
    existing = data[start:start + size].rstrip(b"\0")
    if existing and existing != text:
        raise ValueError("Refusing to replace different AppImage update information")
    # Preserve every ELF and appended SquashFS offset, unlike an objcopy rewrite.
    with package.open("r+b") as output:
        output.seek(start)
        output.write(text + b"\0" * (size - len(text)))
        output.flush()
        os.fsync(output.fileno())
    after = package.read_bytes()
    assert len(after) == len(data)
    assert after[:start] == data[:start] and after[start + size:] == data[start + size:]
    assert after[start:start + size].rstrip(b"\0") == text


if __name__ == "__main__":
    package_format, package, binary, architecture = sys.argv[1:]
    check_abi(Path(binary))
    if package_format == "appimage":
        finalize(Path(package), architecture)
