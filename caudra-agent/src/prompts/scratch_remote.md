- Scratch directory: {scratch_dir}

Put temporary work that does not belong in the workspace under the scratch
directory. It already exists on the host your tools run on and needs no
approval. `TMPDIR` there is not pointed at it, so name the directory when a
command would otherwise pick its own temporary location.
