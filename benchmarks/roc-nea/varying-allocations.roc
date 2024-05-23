app "nea-test"
    packages { pf: "platform/main.roc" }
    imports [pf.Request.{ Request }]
    provides [main] to pf

parseU64 : Str -> U64
parseU64 = \input ->
    when Str.toU64 input is
        Ok v -> v
        Err _ -> crash "invalid input"

main : Request -> Str
main = \input ->
    when Str.splitFirst input.path "/" is
        Ok { before: _, after } -> 
            capacity = parseU64 after * 1024

            x : List U8
            x = List.repeat 0xAA capacity

            Num.toStr (List.len x)

        Err NotFound ->
            crash "invalid input"
