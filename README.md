# *c o s m o s*

---

Cosmos is a terminal-based AI code reviewer. Point it at your project, and it will scan for bugs, performance issues, and code smells. When you find something worth fixing, Cosmos creates a branch, applies the fix, runs an adversarial AI review to catch mistakes, and helps you ship a PR — all without leaving the terminal.

**No IDE required. Minimal setup: install Cosmos and add your OpenRouter API key on first run.**

---

## Who Is Cosmos For?

- **Non-engineers maintaining code** — Cosmos explains issues in plain English and guides you through fixes step by step
- **Technical users who value low friction** — AI-powered code review directly in your terminal, integrated with git
- **Solo developers and small teams** — a second pair of eyes that catches bugs and suggests improvements
- **Anyone learning to code** — understand *why* certain patterns are problematic

---

## What Cosmos Does

- **Scans your codebase** using AST-based indexing to understand your code's structure
- **Identifies improvements** — bugs, performance issues, code quality, and more
- **Explains issues in plain English** — no jargon, just clear descriptions of what's wrong and why it matters
- **Suggests fixes** and lets you preview the plan, scope, and affected files before applying
- **Reviews its own work** — an adversarial AI reviewer double-checks each applied fix for issues
- **Creates pull requests** directly via the GitHub API so changes can go through your normal review process

**Supported languages:** JavaScript, TypeScript, Python, Rust, Go

---

## How Cosmos Works: The 4 Stages

When you apply a fix in Cosmos, it follows a careful 4-stage process to keep your code safe:

### 1. Preview

Before anything changes, Cosmos shows you what the fix is intended to do. You'll see:
- A plain-English summary of the problem
- What will be different after the fix
- Which parts of the code are affected

This is your chance to understand the change and decide whether to proceed.

### 2. Verify [and Apply]

Once you approve, Cosmos creates a new git branch and applies the fix. Your main branch stays untouched. The fix is generated as search-and-replace edits and applied to update file contents, keeping changes focused.

### 3. Review

Here's where Cosmos gets thorough: after applying the fix, it runs an adversarial AI review using a different model. This reviewer's job is to find problems — bugs, edge cases, issues the fix might have introduced. If issues are found, you can select findings to fix; Cosmos can apply those fixes and re-review.

### 4. Ship

When the review passes, you can commit, push, and create a pull request — all from within Cosmos. On first run, Cosmos will guide you through GitHub authentication.

---

## Installation

Installation takes a few minutes (it installs Rust if needed, then compiles Cosmos).

### Mac / Linux

```bash
curl -sSL https://raw.githubusercontent.com/cameronspears/cosmos/main/install.sh | bash
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/cameronspears/cosmos/main/install.ps1 | iex
```

When you see "Installation complete!", you're ready to go.

---

## Getting Started

1. **Navigate to your project** — Cosmos expects a git repository:
   ```bash
   cd /path/to/your/project
   ```

2. **Run Cosmos:**
   ```bash
   cosmos
   ```

3. **Set up your API key (first time only)** — Cosmos uses AI models via [OpenRouter](https://openrouter.ai). See below for setup instructions.

---

## Setting Up Your OpenRouter API Key

Cosmos uses [OpenRouter](https://openrouter.ai) for AI access. You only pay for what you use.

1. **Create an account** at [openrouter.ai](https://openrouter.ai)
2. **Add credits** at [openrouter.ai/credits](https://openrouter.ai/credits)
3. **Create an API key** at [openrouter.ai/keys](https://openrouter.ai/keys) — copy the key (starts with `sk-`)
4. **Paste it in Cosmos** when prompted on first run (stored in your system keychain)

**Alternative:** Set `OPENROUTER_API_KEY` as an environment variable instead.

**To change your key later:** Run `cosmos --setup`

**Costs:** Results are cached locally to minimize repeat calls. Monitor usage at [openrouter.ai/usage](https://openrouter.ai/usage).

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

## Types of Suggestions

Cosmos identifies several categories of improvements:

| Type | What It Finds |
|------|---------------|
| **Bug Fix** | Logic errors, edge cases, null handling, race conditions |
| **Optimization** | Performance issues, N+1 queries, unnecessary allocations |
| **Quality** | Code smells, maintainability issues, error handling gaps |
| **Refactoring** | Opportunities to simplify, extract, or restructure code |
| **Testing** | Missing test coverage, untested edge cases |
| **Documentation** | Unclear code that needs explanation |
| **Feature** | Potential enhancements based on code patterns |

### Suggestion Priority

Cosmos marks suggestions by importance:

| Icon | Meaning |
|------|---------|
| `!!` | High priority — significant improvement or likely bug |
| `!` | Medium priority — worth considering |
| (blank) | Low priority — minor enhancement |

Suggestions are sorted by priority and relevance — issues in files you've recently changed appear first.

---

## Updating Cosmos

Cosmos checks for updates automatically on startup. When a new version is available, you'll see a subtle `U update` indicator in the footer.

**To update:**
1. Press `U` to open the update panel
2. Cosmos will ask: "Would you like to download and install it?"
3. Press `y` to confirm and install, or `n` to decline
4. If you confirm, Cosmos runs `cargo install` to compile the new version and restarts automatically

Updates are completely optional - you can continue working and update whenever you're ready.

**Alternative:** Re-run the install command from the [Installation](#installation) section, or use `cargo install cosmos-tui --force`.

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

### "Command not found"

1. Restart your terminal
2. Run `source ~/.cargo/env` (Mac/Linux)
3. If still not found, re-run the install script

**Mac zsh users:** Add Cargo to PATH: `echo 'source "$HOME/.cargo/env"' >> ~/.zshrc && source ~/.zshrc`

**Windows execution policy error:** Run `Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser` first.

### No suggestions showing

Make sure you're in a git repo with supported files (JavaScript, TypeScript, Python, Rust, Go).

### API key issues

Run `cosmos --setup` to reconfigure. Ensure your key starts with `sk-` and you have credits at [openrouter.ai/credits](https://openrouter.ai/credits).

---

## Quick Reference

```bash
# Run cosmos in current folder
cosmos

# Run cosmos on a specific project
cosmos /path/to/your/project

# Set up or change your API key
cosmos --setup

# Show version
cosmos --version
```

---

## How Cosmos Works Under the Hood

### Indexing

Cosmos indexes your codebase using AST parsing for structural understanding — functions, classes, imports, dependencies. The index is cached in `.cosmos/` so subsequent runs are faster.

### Analysis

Code context is sent to AI models via OpenRouter. Payload size is limited — large files use excerpts, and results are batched for efficiency.

### Fix Generation

Fixes use a two-phase approach:
1. **Preview:** A balanced model verifies the issue and plans the fix
2. **Apply:** A more capable model implements the changes as surgical search-and-replace edits

### Adversarial Review

After applying a fix, a *different* AI model reviews the changes. This cognitive diversity helps catch issues the implementing model might miss.

---

## Privacy & Security

**Sent to AI:** Code context, file paths, symbol names, and project metadata. If secrets are in analyzed files, they can be included.

**Stays local:** Your API key (keychain or env var), cached results (`.cosmos/`), and all git operations until you push.

**Your control:** Run `cosmos --setup` to manage your API key. Delete `.cosmos/` to clear cache. All changes happen on separate branches — approve before applying, review via git diff.

---

## FAQ

**How much does Cosmos cost?**
Free and open source. You pay for AI usage through OpenRouter.

**Can I use Cosmos offline?**
Indexing and caching happen locally, but suggestions and fixes require an internet connection.

**What if I don't like a suggestion?**
Ignore it. Cosmos won't apply changes without your approval.

**Does Cosmos work with private repositories?**
Yes. See the [Privacy & Security](#privacy--security) section for data handling details.

**Can I use my own OpenAI/Anthropic API key?**
Currently, Cosmos works through OpenRouter, which provides access to models from multiple providers through a single API.

---

## Getting Help

- **GitHub Issues:** [github.com/cameronspears/cosmos/issues](https://github.com/cameronspears/cosmos/issues)
- **Discussions:** [github.com/cameronspears/cosmos/discussions](https://github.com/cameronspears/cosmos/discussions)

---

## Contributing

```bash
# Clone and set up the repo
git clone https://github.com/cameronspears/cosmos.git
cd cosmos

# Enable pre-commit hooks (auto-formats code before commits)
git config core.hooksPath .githooks

# Run tests
cargo test
```

---

## License

MIT
