# Changelog

All notable changes to this project will be documented in this file.

## [1.0.3] - 2026-09-27

### CI/CD

- Install the arm64 sysroot for the cross build

## [1.0.2] - 2026-09-27

### CI/CD

- Install the cross target under the pinned toolchain

### Release

- V1.0.2

## [1.0.1] - 2026-09-27

### CI/CD

- Point the arm64 cross build at ports.ubuntu.com

### Documentation

- **readme**: Credit musicforprogramming.net and note the unofficial status
- **readme**: Recolour the badges to the brand palette
- Serve the installer from pivoshenko.dev/mfp.sh

### Release

- V1.0.1

## [1.0.0] - 2026-09-27

### Build

- Carry a version on the mfp-core path dependency
- Add the metadata crates.io publishing requires

### CI/CD

- Run cargo-deny through just audit
- Add CI, release, and install pipelines

### Design

- **assets**: Drop the comments and trim the preview canvas
- **assets**: Redraw the social preview as the analyser band
- **assets**: Add the social preview cards

### Documentation

- Nest the paths table under configuration
- Trim the project docs
- Drop the doc references to the removed spec files
- Add contributor docs and the agent map

### Features

- **cli**: Report the version with --version
- Add the musicforprogramming.net terminal player

### Miscellaneous

- Trim the config comments
- Add editorconfig, git-cliff config, and just recipes

### Testing

- **audio**: Ignore the end-of-stream test without a device

### Release

- V1.0.0

