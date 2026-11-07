# Source provenance

The initial history was extracted with `git subtree split` from
`https://github.com/nonary-streams/amaterasu`:

- Source commit: `9c6ae8dcc546216ff7001a26cac175209ea32041`
- Source path: `modules/remote-session`
- Source tree: `03257f7fa2b4182c3fe1c6ee56c02eff8444e704`
- Split commit: `264456671e998bf8e1302db7bc8bff7ff0a59303`

The 27 extracted commits retain the original author, message and file history.
The split commit's root tree equals the source subtree. Extraction adds explicit
Cargo metadata, licensing, independent CI and repository documentation; the
native source, public metadata, build helpers and UI assets retain their source
bytes. The `core-integration-tests` default and core-only test dependencies remain
for parent workspace validation. The standalone helper omits only the declared
core test dependencies and feature when staging a portable SDK build.

The independently published public SDK validation pin is
`800e1c6e4e20c2f949b3438af27f2f080caa10d6` from
`https://github.com/nonary-streams/amaterasu-sdk`. The native and UI CI jobs
check out that exact revision from `sdk-revision`.
