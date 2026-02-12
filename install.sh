#!/bin/bash
set -e

# Cosmos installer
# Installs Rust (if needed) and builds cosmos from source

REPO="cameronspears/cosmos"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

print_step() {
    echo -e "${BLUE}==>${NC} $1"
}

print_success() {
    echo -e "${GREEN}✓${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}!${NC} $1"
}

print_error() {
    echo -e "${RED}✗${NC} $1"
}

echo ""
echo "  ╭─────────────────────────────────────╮"
echo "  │      Installing cosmos              │"
echo "  ╰─────────────────────────────────────╯"
echo ""

# Detect OS
OS=$(uname -s | tr '[:upper:]' '[:lower:]')

case "$OS" in
    darwin)
        print_step "Detected macOS"
        ;;
    linux)
        print_step "Detected Linux"
        ;;
    *)
        print_error "Unsupported OS: $OS"
        echo ""
        echo "  For Windows, use PowerShell:"
        echo "  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex"
        echo ""
        exit 1
        ;;
esac

# Check for Rust
if command -v cargo &> /dev/null; then
    print_success "Rust is already installed"
    RUST_VERSION=$(rustc --version)
    echo "     $RUST_VERSION"
else
    print_step "Rust is not installed. Installing now..."
    echo ""
    echo "  Rust is the programming language cosmos is built with."
    echo "  This installation is safe and can be removed later with 'rustup self uninstall'."
    echo ""
    
    # Install rustup
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    
    # Source the cargo environment
    source "$HOME/.cargo/env"
    
    if command -v cargo &> /dev/null; then
        print_success "Rust installed successfully"
    else
        print_error "Failed to install Rust"
        echo ""
        echo "  Please install Rust manually: https://rustup.rs"
        echo "  Then run this script again."
        echo ""
        exit 1
    fi
fi

echo ""
print_step "Installing cosmos from crates.io..."
echo ""
echo "  This compiles cosmos for your system. It may take a few minutes."
echo ""

# Install cosmos via cargo (distribution profile for smallest/fastest binary)
# Using --locked to ensure reproducible builds when Cargo.lock is present
if cargo install cosmos-tui --profile release-dist 2>&1; then
    print_success "cosmos installed successfully!"
else
    # If crates.io install fails, try from git
    print_warning "crates.io install failed, trying from GitHub..."
    cargo install --git "https://github.com/$REPO" --package cosmos-tui --locked --profile release-dist
    print_success "cosmos installed successfully!"
fi

echo ""
echo "  ╭─────────────────────────────────────╮"
echo "  │      Installation complete!         │"
echo "  ╰─────────────────────────────────────╯"
echo ""
echo "  To get started:"
echo ""
echo "    1. Open a terminal in your project folder"
echo "    2. Run: cosmos"
echo ""
echo "  If 'cosmos' is not found, restart your terminal or run:"
echo "    source ~/.cargo/env"
echo ""
