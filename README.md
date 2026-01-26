# *c o s m o s*

*A contemplative companion for your codebase*

---

Cosmos is an AI-powered code maintenance tool that lives in your terminal. It scans your project, identifies areas that could be improved, explains issues in plain English, and helps you fix them — all without leaving the command line.

**No IDE required. Minimal setup: install Cosmos and add your OpenRouter API key on first run.**

---

## Who Is Cosmos For?

**Non-engineers maintaining code:** You don't need to be a developer to keep a codebase healthy. Cosmos explains issues in plain English and guides you through fixes step by step.

**Technical users who value low friction:** Skip the context-switching. Cosmos brings AI-powered code review directly to your terminal, integrated with your git workflow.

**Solo developers and small teams:** Get a second pair of eyes on your code. Cosmos acts as an AI code reviewer that catches bugs, suggests improvements, and helps maintain quality.

**Anyone learning to code:** Understand *why* certain patterns are problematic and learn best practices through Cosmos's explanations.

---

## What Cosmos Does

- **Scans your codebase** using AST-based indexing to understand your code's structure
- **Identifies improvements** — bugs, performance issues, code quality, and more
- **Explains issues in plain English** — no jargon, just clear descriptions of what's wrong and why it matters
- **Suggests fixes** and lets you preview the plan, scope, and affected files before applying
- **Reviews its own work** — an adversarial AI reviewer double-checks each applied fix for issues
- **Creates pull requests** via the GitHub CLI (`gh`) so changes can go through your normal review process

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

### 2. Apply

Once you approve, Cosmos creates a new git branch and applies the fix. Your main branch stays untouched. The fix is generated as search-and-replace edits and applied to update file contents, keeping changes focused.

### 3. Review

Here's where Cosmos gets thorough: after applying the fix, it runs an adversarial AI review using a different model. This reviewer's job is to find problems — bugs, edge cases, issues the fix might have introduced. If issues are found, you can select findings to fix; Cosmos can apply those fixes and re-review.

### 4. Ship

When the review passes, you can commit, push, and create a pull request — all from within Cosmos. Pull request creation uses the GitHub CLI (`gh`) and requires it to be installed and authenticated.

---

## Installation

Choose your operating system below. Installation usually takes a few minutes (depends on machine and network) and requires an internet connection.

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
2. If not, it installs Rust automatically — this is safe and can take a few minutes
3. It then compiles and installs cosmos — this can take a few minutes

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
3. It then compiles and installs cosmos — this can take a few minutes

When you see "Installation complete!", you're ready to go.

### Linux

Open your terminal and run:

```bash
curl -sSL https://raw.githubusercontent.com/cameronspears/cosmos/main/install.sh | bash
```

The process is the same as Mac — it installs Rust if needed, then builds cosmos. Timing depends on your machine and network.

---

## Getting Started

### Step 1: Open your project folder in Terminal

Cosmos needs to run from inside your project folder. It expects a git repository (it uses git for branches, undo, and sorting suggestions).

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

Cosmos uses AI models via [OpenRouter](https://openrouter.ai) to analyze your code. The first time you run it, you'll need to set up an API key.

See the detailed instructions in the next section.

---

## Setting Up Your OpenRouter API Key

Cosmos uses OpenRouter as its AI backend. OpenRouter provides access to multiple AI models through a single API, and you only pay for what you use.

### Getting Your API Key

1. **Create an OpenRouter account**
   - Go to [openrouter.ai](https://openrouter.ai)
   - Sign up with your email or GitHub account

2. **Add credits to your account**
   - Navigate to [openrouter.ai/credits](https://openrouter.ai/credits)
   - Add funds (even a small amount is enough to get started; usage varies)
   - See OpenRouter pricing for current rates

3. **Generate an API key**
   - Go to [openrouter.ai/keys](https://openrouter.ai/keys)
   - Click "Create Key"
   - Give it a name like "Cosmos"
   - Copy the key (it starts with `sk-`)

4. **Enter the key in Cosmos**
   - Run `cosmos` in your terminal
   - Paste your key when prompted
   - Cosmos stores it in your system keychain when available

### Alternative: Environment Variable

If you prefer not to use the system keychain, you can set your key as an environment variable:

```bash
export OPENROUTER_API_KEY="sk-your-key-here"
```

Add this to your shell profile (`~/.bashrc`, `~/.zshrc`, etc.) to persist it.

### Changing Your API Key

To update or change your API key at any time:

```bash
cosmos --setup
```

### Cost Expectations

Cosmos uses AI efficiently:
- **Caching:** Results are cached locally to minimize repeat calls
- **Model tiers:** Uses faster models for summaries and stronger models for fixes/reviews
- **Costs vary:** Usage depends on project size and models; check OpenRouter usage for exact cost

You can monitor your usage at [openrouter.ai/usage](https://openrouter.ai/usage).

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

1. **Preview** — Cosmos shows the plan, scope, and affected files
2. **Apply** — Creates a new branch with the fix
3. **Review** — Cosmos checks the applied change for issues
4. **Ship** — Commit, push, and create a pull request (via `gh`)

This keeps your main code safe. You choose if and when to ship changes via your normal review flow.

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

**"Invalid API key" or authentication errors:**
- Make sure your key starts with `sk-`
- Check that you've added credits at [openrouter.ai/credits](https://openrouter.ai/credits)
- Verify your key is active at [openrouter.ai/keys](https://openrouter.ai/keys)

**Keychain access issues on Mac:**
- When macOS prompts, choose "Always Allow" for the "cosmos" app
- Alternatively, use the environment variable: `export OPENROUTER_API_KEY="your-key"`

**Key not persisting:**
- Try the environment variable method instead of keychain storage
- Add to your shell profile for persistence

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

# Show version
cosmos --version
```

---

## How Cosmos Works Under the Hood

For those curious about the technical details:

### Indexing

When you run Cosmos, it first indexes your codebase using AST (Abstract Syntax Tree) parsing. This gives it a structural understanding of your code — functions, classes, imports, dependencies — not just text matching.

The index is cached locally (in `.cosmos/` in your project) so subsequent runs are faster.

### Analysis

Cosmos sends code context to AI models via OpenRouter for summaries, suggestions, previews, fixes, and reviews. It aims to limit payload size:
- Summaries may include full file contents, batched for efficiency
- Previews/fixes/reviews use excerpts or truncation for large files
- File paths, symbol names, and a short project description (from README or package metadata) may be included
- Domain terminology learned from your codebase

### Fix Generation

Fixes use a two-phase approach:
1. **Preview phase:** A balanced model verifies the issue exists and plans the fix
2. **Apply phase:** A more capable model implements the actual code changes

Changes are generated as search-and-replace edits and applied to update file contents, keeping modifications surgical.

### Adversarial Review

After applying a fix, Cosmos uses a *different* AI model to review the changes. This cognitive diversity helps catch issues that the implementing model might have blind spots for.

The reviewer is prompted to be skeptical and adversarial — its job is to find bugs, not praise good code.

---

## Privacy & Security

**What data is sent to AI:**
- Code context for summaries, suggestions, previews, fixes, and reviews
- File paths, file structure, and symbol names for context
- Project context from README/package metadata when available
- Cosmos does not read your shell environment, but if secrets are in code or analyzed files, they can be included

**What stays local:**
- Your API key (stored in your system keychain when available, or via env var)
- Cached analysis results (stored in your project's `.cosmos/` folder)
- Git operations (handled entirely locally until you explicitly push)

**What you control:**
- Run `cosmos --setup` to manage your API key
- Delete `.cosmos/` to clear cached data
- Approve the fix plan before applying, and review actual code changes in the Review step or via git diff
- Changes are applied on a separate branch — your main code is protected

---

## Frequently Asked Questions

**How much does Cosmos cost?**

Cosmos itself is free and open source. You pay for AI usage through OpenRouter; costs vary by model and project size.

**Is my code sent to the cloud?**

Yes, code context is sent to AI models via OpenRouter for summaries, suggestions, previews, fixes, and reviews. Cosmos does not run a server or store your code remotely, but OpenRouter/model providers may retain requests per their policies. See the [Privacy & Security](#privacy--security) section for details.

**Can I use Cosmos offline?**

Cosmos requires an internet connection for AI features. The indexing and caching happen locally, but suggestions and fixes need AI access.

**What if I don't like a suggestion?**

You can ignore any suggestion. Cosmos won't apply changes without your approval, and fixes happen on a separate branch so your main code is protected.

**Does Cosmos work with private repositories?**

Yes. Cosmos runs on your machine. It still sends code context to AI models via OpenRouter; those providers' data retention policies apply.

**What AI models does Cosmos use?**

Cosmos uses a mix of models via OpenRouter, selected for their strengths:
- Fast models for quick categorization
- Balanced models for verification and planning
- Smart models for implementing fixes
- A separate reviewer model for adversarial review

**Can I use my own API key from OpenAI/Anthropic directly?**

Currently, Cosmos works through OpenRouter, which provides access to models from OpenAI, Anthropic, Google, and others through a single API. This gives you flexibility and competitive pricing.

---

## Getting Help

- **GitHub Issues:** [github.com/cameronspears/cosmos/issues](https://github.com/cameronspears/cosmos/issues)
- **Discussions:** [github.com/cameronspears/cosmos/discussions](https://github.com/cameronspears/cosmos/discussions)

---

## License

MIT

---

*"A contemplative companion for your codebase"*
