# Spike: extension relay for #793 (throwaway)

`./run.sh` launches headless Chrome for Testing 156 in a throwaway profile under
`~/.cache/agent-scratch/fastbrowse-ext-spike/run`, loads `ext/` (pinned ID
`ldancfdkkogpfnhdmghknefcpblendmo`), registers `host.py` as a native-messaging host inside that profile, and
runs `client.py` against `fixture/` through the relay and then over direct CDP on the same browser.
Not for merging: the verdict lives on the wayfinder map #784.
