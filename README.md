# Stitch

A shitty ffmpeg wrapper for bulk concatenating video files

## Installation
```bash
git clone https://github.com/charliethomson/stitch && cd stitch && cargo install --path .
```

## Usage
```bash
Usage: stitch [OPTIONS] <SPEC_FILE>

Arguments:
  <SPEC_FILE>  Path to the specification file containing stitch instructions

Options:
  -v, --verbose  Enable verbose logging (configure with RUST_LOG environment variable)
  -h, --help     Print help
  -V, --version  Print version

Directories:
  -o, --target-dir <DIR>   Output directory for stitched video files (default: current directory)
  -i, --sources-dir <DIR>  Input directory containing source video files (default: current directory)
```

### Example
```bash
# Basic usage
stitch example.stitchspec

# With custom directories
stitch example.stitchspec -i ./raw -o ./output

# With verbose logging
RUST_LOG=debug stitch example.stitchspec -v
```

## Specification Format

```yaml
video.mp4:
    part_1.mp4
    part_2.mp4
    part_3.mp4
```

## Requirements

- `ffmpeg` and `ffprobe` must be installed and available in your PATH

## License

[Your License Here]
