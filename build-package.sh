#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-only
# Helper script to build and optionally install the Arch package

set -e

echo "Building q6w package..."
echo ""

# Show current version that will be built
COMMIT_COUNT=$(git rev-list --count HEAD)
COMMIT_HASH=$(git rev-parse --short=7 HEAD)
echo "Package version: 0.1.$COMMIT_COUNT"
echo "Commit hash: $COMMIT_HASH"
echo ""

# Build package
makepkg -f

echo ""
echo "Package built successfully!"
echo ""
echo "To install: makepkg -si"
echo "To generate .SRCINFO for AUR: makepkg --printsrcinfo > .SRCINFO"
