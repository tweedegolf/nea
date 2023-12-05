app "nea-test"
    packages { pf: "platform/main.roc" }
    imports [pf.Request.{ Request }]
    provides [main] to pf

parseNat : Str -> Nat
parseNat = \input ->
    when Str.toNat input is
        Ok v -> v
        Err _ -> crash "invalid input"

main : Request -> Str
main = \input ->
    when Str.splitFirst input.path "/" is
        Ok { before: _, after } -> 
            capacity = parseNat after * 1024

            x : List U8
            x = List.withCapacity capacity

            Num.toStr (List.len x)

        Err NotFound ->
            crash "invalid input"
