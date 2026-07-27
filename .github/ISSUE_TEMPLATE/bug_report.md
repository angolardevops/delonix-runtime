---
name: Bug report
about: Something doesn't work the way it should
title: ""
labels: bug
assignees: ""
---

**`delonix --version`**

```
(paste output here)
```

**Environment**
- Distro / kernel version:
- Rootless or root:
- Installed via `install.sh`, a manual binary download, or built from source:

**What did you run?**

```
(the exact delonix command, or manifest, that triggers this)
```

**What did you expect to happen?**

**What actually happened?**

```
(full output/error — do not trim it, the exact wording usually matters here)
```

**Can you reproduce it reliably?**
- [ ] Yes, every time
- [ ] Sometimes
- [ ] Only once, not sure how to reproduce

**Anything else that might be relevant?** (a previous crash, unusual host config, a container
image with unusual entrypoint behavior, etc.)

---

If this looks like a security vulnerability (privilege escalation, namespace escape, command
injection, path traversal) — please **don't** file it here. See
[SECURITY.md](../../SECURITY.md) for private disclosure instead.
