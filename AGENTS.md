# AGENTS.md


## Core Principles

- Surface tradeoffs explicitly.
- Never hide uncertainty or confusion.
- Prefer simple, direct solutions over clever abstractions.
- Avoid premature generalization and overengineering.
- Be concrete. Use real examples and exact failure modes.
- Verify behavior empirically whenever possible.


## Coding Style

- Write straightforward, readable code.
- Avoid unnecessary indirection.
- Prefer explicit data flow over hidden state.
- Use comments to explain *why*, not *what*.
- Fail loudly and early.
- Treat warnings as errors.


## Communication

- Be concise and direct.
- Do not oversell solutions.
- If confidence is low, say so clearly.
- Stick to ASCII characters.


## Workflow

Before committing, always run:
- Formatter
- Linter
- Tests

For commit messages:
- Title should follow the repo standard
- Message body should contain:
  1. The request summarized.
  2. A summary of what was actually implemented.
