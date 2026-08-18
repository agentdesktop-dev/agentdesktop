export function macosArchitecture(target, nativeArchitecture = process.arch) {
  const architecture = target?.split("-", 1)[0] ?? nativeArchitecture;
  switch (architecture) {
    case "aarch64":
    case "arm64":
      return "arm64";
    case "x86_64":
    case "x64":
      return "amd64";
    default:
      throw new Error(`unsupported macOS architecture: ${architecture}`);
  }
}
