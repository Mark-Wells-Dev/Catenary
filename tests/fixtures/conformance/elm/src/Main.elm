module Main exposing (main)

import Html exposing (Html, text)


-- Conformance fixture (tui-rework 07).
-- Intentional diagnostic: a type mismatch — `text` expects a String but is
-- given an Int; elm-language-server surfaces the elm compiler error.
main : Html msg
main =
    text 42
