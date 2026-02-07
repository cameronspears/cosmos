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

# Check for Rust - prefer PATH, fall back to user install location
$cargoInPath = Get-Command cargo -ErrorAction SilentlyContinue
$cargoUserPath = "$env:USERPROFILE\.cargo\bin\cargo.exe"

if ($cargoInPath) {
    # Cargo is in PATH (e.g., CI environments, or user has it in PATH)
    $cargoCmd = "cargo"
    Write-Success "Rust is already installed"
    try {
        $rustVersion = & rustc --version 2>$null
        Write-Host "     $rustVersion"
    } catch {}
} elseif (Test-Path $cargoUserPath) {
    # Cargo installed in user directory but not in PATH
    $cargoCmd = $cargoUserPath
    Write-Success "Rust is already installed"
    try {
        $rustVersion = & "$env:USERPROFILE\.cargo\bin\rustc.exe" --version 2>$null
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
    Write-Host "  Accept the default options (just press Enter) when prompted."
    Write-Host ""
    
    Start-Process -FilePath $rustupInit -ArgumentList "-y" -Wait
    
    # Update PATH for current session
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    $cargoCmd = $cargoUserPath
    
    # Verify installation
    if (Test-Path $cargoUserPath) {
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

# Install cosmos via cargo (distribution profile for smallest/fastest binary)
$installSuccess = $false

# Try crates.io first
& $cargoCmd install cosmos-tui --profile release-dist 2>&1
if ($LASTEXITCODE -eq 0) {
    $installSuccess = $true
} else {
    Write-Warn "crates.io install failed, trying from GitHub..."
    & $cargoCmd install --git "https://github.com/$REPO" --locked --profile release-dist 2>&1
    if ($LASTEXITCODE -eq 0) {
        $installSuccess = $true
    }
}

if (-not $installSuccess) {
    Write-Err "Installation failed"
    Write-Host ""
    Write-Host "  Please try manually: cargo install cosmos-tui"
    Write-Host ""
    exit 1
}

Write-Success "cosmos installed successfully!"

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
