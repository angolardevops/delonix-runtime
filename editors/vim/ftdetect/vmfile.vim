" A VMfile is recognised by its NAME, not by an extension — it has none, the
" same way a Dockerfile has none. `VMfile.dev`/`dev.VMfile` are the two shapes
" people reach for when a project holds more than one.
au BufNewFile,BufRead VMfile,VMfile.*,*.VMfile,*.vmfile setfiletype vmfile
