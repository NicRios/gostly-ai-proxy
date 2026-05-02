---

## Install

**Homebrew (macOS, Linux):**
```bash
brew tap nicrios/gostly https://github.com/NicRios/gostly-ai-proxy
brew install gostly
```

**Scoop (Windows):**
```powershell
scoop bucket add gostly https://github.com/NicRios/gostly-ai-proxy
scoop install gostly
```

**Linux/macOS one-liner:**
```bash
curl -fsSL https://raw.githubusercontent.com/NicRios/gostly-ai-proxy/main/install.sh | bash
```

**Docker:**
```bash
docker pull ghcr.io/nicrios/gostly-proxy:latest
docker run --rm -p 8080:8080 ghcr.io/nicrios/gostly-proxy:latest --help
```
