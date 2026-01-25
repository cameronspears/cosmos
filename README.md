# *c o s m o s*

An AI-powered assistant that reviews your code and suggests improvements — right in your terminal.

Cosmos reads your project, finds things that could be better, and helps you fix them. No complex setup. No IDE required. Just run it and go.

## What Cosmos Does

- **Scans your code** and finds areas that could be improved
- **Explains issues in plain English** — no jargon
- **Suggests fixes** and lets you preview changes before applying
- **Creates pull requests** so changes go through your normal review process

**Supported languages:** JavaScript, TypeScript, Python, Rust, Go

---

## Installation

Choose your operating system below. Installation takes about 5 minutes and requires an internet connection.

### Mac

Open the **Terminal** app and paste this command:

```bash
curl -sSL https://raw.githubusercontent.com/cameronspears/cosmos/main/install.sh | bash
```

**How to open Terminal:**
1. Press `Cmd + Space` to open Spotlight
2. Type "Terminal" and press Enter

**What happens during installation:**
1. The installer checks if you have Rust (a programming language) installed
2. If not, it installs Rust automatically — this is safe and takes about 2 minutes
3. It then compiles and installs cosmos — this takes another 2-3 minutes

When you see "Installation complete!", you're ready to go.

### Windows

Open **PowerShell** and paste this command:

```powershell
irm https://raw.githubusercontent.com/cameronspears/cosmos/main/install.ps1 | iex
```

**How to open PowerShell:**
1. Press the Windows key
2. Type "PowerShell" and press Enter

**What happens during installation:**
1. The installer checks if you have Rust (a programming language) installed
2. If not, it downloads and runs the Rust installer — just press Enter to accept defaults
3. It then compiles and installs cosmos — this takes about 2-3 minutes

When you see "Installation complete!", you're ready to go.

### Linux

Open your terminal and run:

```bash
curl -sSL https://raw.githubusercontent.com/cameronspears/cosmos/main/install.sh | bash
```

The process is the same as Mac — it installs Rust if needed, then builds cosmos.

---

## Getting Started

### Step 1: Open your project folder in Terminal

Cosmos needs to run from inside your project folder.

**Mac:**
1. Open Terminal
2. Type `cd ` (with a space after it)
3. Drag your project folder from Finder into the Terminal window
4. Press Enter

**Windows:**
1. Open PowerShell
2. Type `cd ` (with a space after it)
3. Type the path to your project, like `C:\Users\YourName\Projects\my-app`
4. Press Enter

**Linux:**
1. Open your terminal
2. Navigate to your project: `cd /path/to/your/project`

**Example:**
```bash
cd /Users/yourname/Projects/my-website
```

### Step 2: Run cosmos

Once you're in your project folder, type:

```bash
cosmos
```

### Step 3: Set up your API key (first time only)

Cosmos uses AI to analyze your code and suggest improvements. The first time you run it, you'll need to set up an API key.

1. Cosmos will show you a link to get an API key
2. Follow the link, create an account if needed, and copy your key
3. Paste the key when cosmos asks for it

Your key is saved securely on your computer. You won't need to enter it again.

---

## Using Cosmos

When cosmos starts, you'll see a list of suggestions for your project.

### Navigation

| Key | What it does |
|-----|--------------|
| `↑` `↓` | Move up and down the list |
| `Enter` | View details or apply a suggestion |
| `Tab` | Switch between panels |
| `?` | Show help |
| `q` | Quit cosmos |

### Working with suggestions

1. **Browse suggestions** — Use arrow keys to look through the list
2. **View details** — Press Enter on any suggestion to see more
3. **Apply a fix** — When viewing a suggestion, press Enter to preview and apply
4. **Undo** — Press `u` to undo the last change

### Other features

| Key | What it does |
|-----|--------------|
| `/` | Search through suggestions |
| `i` | Ask cosmos a question about your code |
| `g` | Toggle between grouped and flat view |
| `Esc` | Go back or cancel |

---

## How Fixes Work

When you apply a suggestion:

1. **Preview** — Cosmos shows you exactly what will change
2. **Apply** — Creates a new branch with the fix
3. **Review** — Cosmos checks the fix for any issues
4. **Ship** — Commit, push, and create a pull request

This keeps your main code safe. All changes go through your normal review process.

---

## Suggestion Priority

Cosmos marks suggestions by importance:

| Icon | Meaning |
|------|---------|
| `!!` | High priority — significant improvement |
| `!` | Medium priority — worth considering |
| (blank) | Low priority — minor enhancement |

---

## Updating Cosmos

To get the latest version, run the install command again:

**Mac/Linux:**
```bash
curl -sSL https://raw.githubusercontent.com/cameronspears/cosmos/main/install.sh | bash
```

**Windows:**
```powershell
irm https://raw.githubusercontent.com/cameronspears/cosmos/main/install.ps1 | iex
```

Or if you're comfortable with Rust, simply run:
```bash
cargo install cosmos-tui
```

---

## Uninstalling

**Remove cosmos:**
```bash
cargo uninstall cosmos-tui
```

**Remove Rust (optional):**
```bash
rustup self uninstall
```

---

## Troubleshooting

### "Command not found" when running cosmos

**Try these steps:**

1. **Restart your terminal** — Close the terminal window and open a new one
2. **Source your shell profile** — Run `source ~/.cargo/env` (Mac/Linux)
3. **Check if cosmos is installed** — Run `which cosmos` (Mac/Linux) or `where cosmos` (Windows)

If cosmos still isn't found, the installation may not have completed. Try running the install script again.

### Installation fails or takes too long

The installation compiles cosmos from source, which requires downloading and building dependencies. This can take several minutes, especially on slower internet connections or older computers.

If installation fails:
1. Make sure you have a stable internet connection
2. Try running the install script again
3. If you see specific error messages, search for them online or open an issue on GitHub

### Cosmos shows no suggestions

Make sure you're running cosmos from inside a project folder that contains code files. Cosmos works with JavaScript, TypeScript, Python, Rust, and Go files.

### API key issues

If cosmos can't find your API key, you can set it up again:

```bash
cosmos --setup
```

### Problems on Mac: "zsh: command not found"

If you're using zsh (the default on newer Macs), you may need to add Cargo to your PATH:

```bash
echo 'source "$HOME/.cargo/env"' >> ~/.zshrc
source ~/.zshrc
```

### Problems on Windows: execution policy error

If PowerShell blocks the install script, run this first:

```powershell
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
```

Then try the install command again.

---

## Quick Reference

```bash
# Run cosmos in current folder
cosmos

# Run cosmos on a specific project
cosmos /path/to/your/project

# Set up or change your API key
cosmos --setup

# Show project statistics
cosmos --stats

# Show version
cosmos --version
```

---

## Privacy

- Your code is sent to the AI service only when generating suggestions
- Your API key is stored securely in your system's keychain
- Cosmos caches results locally to minimize API usage and costs

---

## Getting Help

- **GitHub Issues:** [github.com/cameronspears/cosmos/issues](https://github.com/cameronspears/cosmos/issues)
- **Discussions:** [github.com/cameronspears/cosmos/discussions](https://github.com/cameronspears/cosmos/discussions)

---

## License

MIT

---

*"A contemplative companion for your codebase"*
