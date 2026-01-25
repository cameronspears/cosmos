# Cosmos installer for Windows
# Installs Rust (if needed) and builds cosmos from source

$ErrorActionPreference = "Stop"

$REPO = "cameronspears/cosmos"

function Write-Step { param($msg) Write-Host "==> " -ForegroundColor Blue -NoNewline; Write-Host $msg }
function Write-Success { param($msg) Write-Host "[OK] " -ForegroundColor Green -NoNewline; Write-Host $msg }
function Write-Warn { param($msg) Write-Host "[!] " -ForegroundColor Yellow -NoNewline; Write-Host $msg }
function Write-Err { param($msg) Write-Host "[X] " -ForegroundColor Red -NoNewline; Write-Host $msg }

Write-Host ""
Write-Host "  +-------------------------------------+"
Write-Host "  |      Installing cosmos              |"
Write-Host "  +-------------------------------------+"
Write-Host ""

# Check for Rust
$cargoPath = "$env:USERPROFILE\.cargo\bin\cargo.exe"
$hasRust = (Get-Command cargo -ErrorAction SilentlyContinue) -or (Test-Path $cargoPath)

if ($hasRust) {
    Write-Success "Rust is already installed"
    
    # Try to get version
    try {
        $rustVersion = & rustc --version 2>$null
        Write-Host "     $rustVersion"
    } catch {}
} else {
    Write-Step "Rust is not installed. Installing now..."
    Write-Host ""
    Write-Host "  Rust is the programming language cosmos is built with."
    Write-Host "  This installation is safe and can be removed later."
    Write-Host ""
    
    # Download and run rustup-init
    $rustupInit = "$env:TEMP\rustup-init.exe"
    
    Write-Step "Downloading Rust installer..."
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustupInit
    
    Write-Step "Running Rust installer..."
    Write-Host ""
    Write-Host "  A new window will open. Accept the default options (just press Enter)."
    Write-Host ""
    
    Start-Process -FilePath $rustupInit -ArgumentList "-y" -Wait
    
    # Update PATH for current session
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    
    # Verify installation
    if (Test-Path $cargoPath) {
        Write-Success "Rust installed successfully"
    } else {
        Write-Err "Failed to install Rust"
        Write-Host ""
        Write-Host "  Please install Rust manually: https://rustup.rs"
        Write-Host "  Then run this script again."
        Write-Host ""
        exit 1
    }
}

Write-Host ""
Write-Step "Installing cosmos from crates.io..."
Write-Host ""
Write-Host "  This compiles cosmos for your system. It may take a few minutes."
Write-Host ""

# Install cosmos via cargo
try {
    & "$env:USERPROFILE\.cargo\bin\cargo.exe" install cosmos-tui
    Write-Success "cosmos installed successfully!"
} catch {
    Write-Warn "crates.io install failed, trying from GitHub..."
    try {
        & "$env:USERPROFILE\.cargo\bin\cargo.exe" install --git "https://github.com/$REPO" --locked
        Write-Success "cosmos installed successfully!"
    } catch {
        Write-Err "Installation failed"
        Write-Host ""
        Write-Host "  Please try manually: cargo install cosmos-tui"
        Write-Host ""
        exit 1
    }
}

Write-Host ""
Write-Host "  +-------------------------------------+"
Write-Host "  |      Installation complete!         |"
Write-Host "  +-------------------------------------+"
Write-Host ""
Write-Host "  To get started:"
Write-Host ""
Write-Host "    1. Open PowerShell in your project folder"
Write-Host "    2. Run: cosmos"
Write-Host ""
Write-Host "  If 'cosmos' is not found, restart PowerShell to refresh your PATH."
Write-Host ""
