# Contributing to rustyfarian-network

Thanks for your interest in contributing!  
This project is maintained by the *rustyfarians* (Rust enthusiasts around datenkollektiv)
and is meant to be **fun, practical, and hackable**.

All kinds of contributions are welcome — code, documentation, bug reports, ideas, or small cleanups.

---

## 🚀 How to Contribute

### 1. Fork & Branch
- Fork the repository
- Create a feature branch from `main`
- Keep changes focused and small where possible

### 2. Make Your Changes
- Follow existing code style where reasonable
- Prefer clarity over cleverness
- Avoid breaking existing behavior unless discussed

### 3. Open a Pull Request
- Describe **what** you changed and **why**
- If the change is visible or behavioral, mention it explicitly
- If it’s cleanup-only, say so clearly

---

## 🧹 “Boy Scout Pass” (Cleanup Changes)

We sometimes refer to a **“Boy Scout pass”**, inspired by the Boy Scout Rule:

> *Always leave the code a little cleaner than you found it.*

In this project, a **Boy Scout pass** means:
- Small cleanups or refactoring
- Improved readability or structure
- No intentional behavior changes

Examples:
- Renaming variables for clarity
- Removing dead code
- Formatting or small refactors

When in doubt, label or describe your change as:
- `cleanup`
- `refactor`
- `boy scout pass (no behavior change)`

---

## 🧪 Testing

If your change affects behavior:
- Pure logic in `rustyfarian-network-pure` can be tested on the host with `cargo test`
- Changes to the ESP-IDF crates require an ESP32 board; mention what you tested (or couldn't test) in the PR description

If your change is cleanup-only:
- Run `just verify` locally and confirm it passes

---

## 📝 Commit Messages

We prefer simple, descriptive commit messages.
No need to be overly formal — just be clear.

---

## 💬 Communication & Conduct

- Be respectful and friendly
- Assume good intent
- Keep discussions technical and constructive

This is an open-source hobby project — let’s keep it enjoyable.

---

## ❓ Questions or Ideas?

- Open an issue
- Start a discussion
- Or just submit a PR and see what happens 🙂

Thanks for helping make `rustyfarian-network` better!
