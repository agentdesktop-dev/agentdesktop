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

/**
 * The macOS Installer's code for a package architecture from
 * `macosArchitecture`, as listed in a distribution's `hostArchitectures`.
 */
export function installerHostArchitecture(architecture) {
  switch (architecture) {
    case "arm64":
      return "arm64";
    case "amd64":
      return "x86_64";
    default:
      throw new Error(
        `unsupported macOS package architecture: ${architecture}`,
      );
  }
}

/**
 * Returns `distribution` with `hostArchitectures` set to the package's
 * architecture on its `<options/>` element.
 *
 * Without arm64 in that list, the macOS Installer on Apple silicon evaluates
 * the distribution under Rosetta 2. On a Mac without Rosetta 2 it then refuses
 * the package ("This package requires Rosetta 2 to be installed"), although
 * the arm64 app runs natively (see `man productbuild`, "arch").
 */
export function distributionForArchitecture(distribution, architecture) {
  const host = installerHostArchitecture(architecture);
  const options = distribution.match(/<options\b[^>]*\/>/g);
  if (options?.length !== 1) {
    throw new Error(
      "distribution.xml must have exactly one <options/> element",
    );
  }
  if (/\bhostArchitectures=/.test(options[0])) {
    throw new Error(
      "distribution.xml must not set hostArchitectures; packaging sets it for each architecture",
    );
  }
  return distribution.replace(
    options[0],
    options[0].replace(/\s*\/>$/, ` hostArchitectures="${host}"/>`),
  );
}
