# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com)
and this project adheres to [Semantic Versioning](http://semver.org).

## [Unreleased]

## [0.3.0] - 2022-12-17

### Added
- Add `MuxedLines::add_file_from_start`.

### Changed
- Update `notify` to `5.0.0`
- Bump MSRV from 1.47 to 1.60, to take advantage of `dep:` syntax
- Bump Rust edition to 2021

## [0.2.4] - 2022-08-15

### Changed
- Muxed events: include paths in remove events
- Update `notify` to `5.0.0-pre.16` to allow `kqueue` support
- Switch OSX notification backend from `fsevents` to `kqueue`
- Store `Watcher` as a trait object to allow runtime variance of the
  notification backend.

## [0.2.3] - 2021-07-31

### Fixed
- Update `notify` to `5.0.0-pre.11` to fix build errors

## [0.2.2] - 2021-06-06

### Fixed
- Properly handle renaming to a watched file, and fix panic when checking
  nonexistent reader position.

## [0.2.1] - 2021-04-23

- Marker release only (no functional changes)

## [0.2.0] - 2021-04-18

### Changed
- Update Tokio dependency to 1.0
- Switch to using `futures_util` for Streams
- Bump MSRV to 1.47 per `notify` update
- Make tokio optional (but default) to allow for future runtime variance.
- `MuxedEvents::add_file` is async and takes `Into<PathBuf>`

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

[Unreleased]: https://github.com/jmagnuson/linemux/compare/0.2.4...master
[0.2.4]: https://github.com/jmagnuson/linemux/compare/0.2.3...0.2.4
[0.2.3]: https://github.com/jmagnuson/linemux/compare/0.2.2...0.2.3
[0.2.2]: https://github.com/jmagnuson/linemux/compare/0.2.1...0.2.2
[0.2.1]: https://github.com/jmagnuson/linemux/compare/0.2.0...0.2.1
[0.2.0]: https://github.com/jmagnuson/linemux/compare/0.1.3...0.2.0
[0.1.3]: https://github.com/jmagnuson/linemux/compare/0.1.2...0.1.3
[0.1.2]: https://github.com/jmagnuson/linemux/compare/0.1.1...0.1.2
[0.1.1]: https://github.com/jmagnuson/linemux/compare/0.1.0...0.1.1
[0.1.0]: https://github.com/jmagnuson/linemux/compare/8a30f75...0.1.0
