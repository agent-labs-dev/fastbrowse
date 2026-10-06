// Run as its own process, so a test can read what the server's stderr put on this process's stderr.
// Arguments: the binary, then optionally `ignore` for the stderr option.

import { Fastbrowse } from '../../src/index.ts';

const [binaryPath, stderr] = process.argv.slice(2);
const fb = await Fastbrowse.start({ binaryPath: binaryPath!, ...(stderr === 'ignore' && { stderr }) });
await fb.close();
