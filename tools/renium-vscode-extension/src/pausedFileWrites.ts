export type FileWriteControlRequest = {
  paths?: string[];
  fileWrites: "pause" | "resume";
};

type FileWriteControl = (request: FileWriteControlRequest) => PromiseLike<unknown>;

export async function withPausedFileWrites<T>(
  control: FileWriteControl,
  write: () => Promise<T>,
  finish: (result: T) => Promise<string[]>,
): Promise<T> {
  let paused = false;
  try {
    await control({ fileWrites: "pause" });
    paused = true;
    const result = await write();
    const settledPaths = await finish(result);
    await control({ paths: settledPaths, fileWrites: "resume" });
    return result;
  } catch (error) {
    if (!paused) {
      throw error;
    }
    try {
      await control({ fileWrites: "resume" });
    } catch (resumeError) {
      const original = error instanceof Error ? error.message : String(error);
      const resume = resumeError instanceof Error ? resumeError.message : String(resumeError);
      throw new Error(`${original}; live sync also failed to resume: ${resume}`);
    }
    throw error;
  }
}
