# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/Skepfyr/little-loadshedder/compare/v0.1.0...v0.2.0) - 2024-02-24

### Fixed
- \[**breaking**\] Swap from returning a LoadShedError to a LoadShedResponse.  
  This makes including the load shedder as a layer much easier,
  as it's simple to map the response as needed but fundamentally
  impossible to map the error as you can't do anything sensible
  if `poll_ready` returned overload.
