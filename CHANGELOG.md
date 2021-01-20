# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com)
and this project adheres to [Semantic Versioning](http://semver.org).

## [Unreleased]

### Changed
- Update Tokio dependency to 1.0
- Switch to using `futures_util` for Streams

## [0.1.3] - 2020-11-22

### Added
- Add `MuxedEvents::next_event`.
- Add `MuxedLines::next_line`.
- Establish 1.40 MSRV.

### Fixed
- Force unwatch on `Rename(Name)` event.

## [0.1.2] - 2020-11-08

### Fixed
- Fix issue where `MuxedLines::add_file` can panic if called while in transient
  `StreamState`.
- Force unwatch on `Remove(File)` event to fix potential race with underlying
  filesystem state.

## [0.1.1] - 2020-04-16

### Added
- Add `Send` + `Sync` to `MuxedLines`.

## [0.1.0] - 2020-04-10

### Added
- Initial library features

[Unreleased]: https://github.com/jmagnuson/linemux/compare/0.1.3...master
[0.1.3]: https://github.com/jmagnuson/linemux/compare/0.1.2...0.1.3
[0.1.2]: https://github.com/jmagnuson/linemux/compare/0.1.1...0.1.2
[0.1.1]: https://github.com/jmagnuson/linemux/compare/0.1.0...0.1.1
[0.1.0]: https://github.com/jmagnuson/linemux/compare/8a30f75...0.1.0
