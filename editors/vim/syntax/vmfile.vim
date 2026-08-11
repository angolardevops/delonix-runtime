" Vim/Neovim syntax for VMfile — delonix's recipe for a bootable qcow2.
"
" The instruction set is the one `cmd/vmfile.rs` parses, and nothing else: the
" parser fails closed on a word it does not know, so an instruction that
" highlights here but does not exist there would be the editor promising a build
" that cannot happen. Keep the two lists in step.
"
" Matching is case-INSENSITIVE at the start of a line because the parser
" uppercases the first word (`line.to_ascii_uppercase()`) — `from ubuntu:24.04`
" builds exactly like `FROM ubuntu:24.04`, so it has to look like it too.

if exists("b:current_syntax")
  finish
endif

" A comment is only a comment on its OWN line — the same rule the parser
" follows, because `RUN sed -i 's/#foo/bar/' x` is ordinary shell and stripping
" from the `#` would mutilate it.
syn match   vmfileComment     "^\s*#.*$" contains=vmfileTodo
syn keyword vmfileTodo        TODO FIXME XXX NOTE contained

" The stage-opening instruction, so `FROM x AS builder` reads as one thing.
syn match   vmfileFrom        "^\s*\c\<from\>"     nextgroup=vmfileImage skipwhite
syn match   vmfileImage       "\S\+"               contained
syn match   vmfileAs          "\s\c\<as\>\s\+\S\+"

" Everything else, at the start of a line.
syn match   vmfileInstruction "^\s*\c\<\%(run\|copy\|env\|user\|password\|rootpassword\|cloudinit\|sshkey\|size\|hostname\|vcpus\|memory\|hypervisor\|label\)\>"

" `COPY --from=<stage>` — the one flag the format has.
syn match   vmfileOption      "--from=\S\+"

" A trailing backslash joins the next line; seeing it is how you notice it is
" missing.
syn match   vmfileContinuation "\\$"

syn region  vmfileString      start=+"+ skip=+\\"+ end=+"+ oneline
syn region  vmfileString      start=+'+ skip=+\\'+ end=+'+ oneline
syn match   vmfileNumber      "\<\d\+[GMK]\?\>"

hi def link vmfileComment      Comment
hi def link vmfileTodo         Todo
hi def link vmfileFrom         Keyword
hi def link vmfileInstruction  Keyword
hi def link vmfileAs           Keyword
hi def link vmfileImage        String
hi def link vmfileOption       Special
hi def link vmfileContinuation Special
hi def link vmfileString       String
hi def link vmfileNumber       Number

let b:current_syntax = "vmfile"
