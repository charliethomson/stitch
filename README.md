# Stitch

A shitty ffmpeg wrapper for bulk concatenating video files

## Installation
```bash
git clone https://github.com/charliethomson/stitch && cd stitch && cargo install --path .
```

## Usage
```bash
stitch [OPTIONS] <SPEC_FILE>

Arguments:
  <SPEC_FILE>  Path to the specification file containing stitch instructions

Options:
  -v, --verbose  Enable verbose logging (configure with RUST_LOG environment variable)
  -h, --help     Print help
  -V, --version  Print version

Directories:
  -o, --target-dir <DIR>   Output directory for stitched video files (default: current directory)
  -i, --sources-dir <DIR>  Input directory containing source video files (default: current directory)

Environment:
      --ffmpeg-path <FFMPEG_PATH>    [env: STITCH_BIN_FFMPEG=]
      --ffprobe-path <FFPROBE_PATH>  [env: STITCH_BIN_FFPROBE=]
```

### Example
```bash
# Basic usage
stitch example.stitchspec

# With custom directories
stitch example.stitchspec -i ./raw -o ./output

# With verbose logging
RUST_LOG=debug stitch example.stitchspec -v

# With specific ffmpeg installation
RUST_LOG=debug STITCH_BIN_FFMPEG=/path/to/bin/ffmpeg STITCH_BIN_FFPROBE=/path/to/bin/ffprobe stitch example.stitchspec -v
```

## Specification Format

```yaml
<output_file>: <flags>
    <input_file_1>
    <input_file_2>
    <input_file_3>
```

## Flags
| Long | Short | Description |
| - | - | - |
| (default) | (default) | The default behavior, uses the [concat demuxer](https://trac.ffmpeg.org/wiki/Concatenate#demuxer), works in most cases |
| `concat-filter` | `catf` | Uses the [concat filter](https://trac.ffmpeg.org/wiki/Concatenate#filter)

```yaml
video.mp4:
	part_1.mp4
	part_2.mp4
	part_3.mp4

video_filter.mp4: catf
	part_1.mp4
	part_2.mp4
	part_3.mp4
```

## Requirements

- `ffmpeg` and `ffprobe` must be available

## License

[Your License Here]
