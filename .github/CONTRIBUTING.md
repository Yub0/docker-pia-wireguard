# Contributing

## Conventional Commits

This project uses [Conventional Commits](https://www.conventionalcommits.org/) for automatic versioning and changelog generation.

### Commit Message Format

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

### Types

| Type | Description | Version Impact |
|------|-------------|----------------|
| `feat` | New feature | Minor (0.x.0) |
| `fix` | Bug fix | Patch (x.x.1) |
| `perf` | Performance improvement | Patch |
| `refactor` | Code refactoring | Patch |
| `build` | Build system changes | Patch |
| `ci` | CI/CD changes | Patch |
| `docs` | Documentation | None (hidden) |
| `style` | Code style changes | None (hidden) |
| `chore` | Maintenance tasks | None (hidden) |

### Breaking Changes

Add `BREAKING CHANGE:` in the footer or use `!` after the type:

```
feat(auth)!: remove JWT support

BREAKING CHANGE: JWT authentication is no longer supported.
```

This will trigger a major version bump (x.0.0).

### Examples

```bash
# Feature (minor version bump)
git commit -m "feat: add port forwarding support"

# Bug fix (patch version bump)
git commit -m "fix: handle connection timeout correctly"

# Breaking change (major version bump)
git commit -m "feat!: new configuration format"
git commit -m "fix(config)!: change default port"
```

## Release Process

Releases are automated using [Release Please](https://github.com/googleapis/release-please):

1. Merge PRs with conventional commits to `main`
2. Release Please creates/updates a release PR
3. When ready, merge the release PR
4. A new version is tagged and published to Docker Hub

## Docker Tags

- `latest` - Latest stable release
- `1.x.x` - Specific version
- `1.x` - Latest patch for minor version
- `1` - Latest patch for major version
- `<sha>` - Git commit SHA (for debugging)
