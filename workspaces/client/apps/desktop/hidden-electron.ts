// Hidden native checks can run on a server with no display connection.
export const hiddenElectronArguments =
  process.platform === "linux" && !process.env.DISPLAY
    ? ["--ozone-platform=headless"]
    : [];
