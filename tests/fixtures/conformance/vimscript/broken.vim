" Conformance fixture (tui-rework 13).
" Intentional diagnostic: vim-language-server derives diagnostics from `vint`
" (a conformance job provisions it). `vint` flags the undefined variable `undef`
" used with no scope prefix inside a function (ProhibitImplicitScopeVariable /
" undefined-variable), so the fixture publishes a diagnostic.
function! s:Main() abort
  return undef
endfunction
