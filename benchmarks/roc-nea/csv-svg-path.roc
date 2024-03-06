app "nea-test"
    packages { pf: "platform/main.roc" }
    imports [pf.Request.{ Request }]
    provides [main] to pf

go : Str, Str -> Str
go = \body, foobar ->
    body
    |> Str.split "\n"
    |> List.map \line ->
        when Str.split line ", " is
            [ xStr, yStr ] ->
                x =
                    when Str.toU32 xStr is
                        Ok v -> v
                        Err _ -> crash "could not parse number"

                y =
                    when Str.toU32 yStr is
                        Ok v -> v
                        Err _ -> crash "could not parse number"

                (x, y)
            _ -> crash "could not split line: $(Num.toStr (Str.countUtf8Bytes line))"
    |> List.walk foobar \accum, (x, y) -> "$(accum)$(Num.toStr x) $(Num.toStr y) "

main : Request -> Str
main = \input ->
    input.body
    |> go (
        """
        <svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
          <path d="M 0 0 L"""
        |> Str.reserve 4096)
    |> Str.concat
        """" stroke="black" fill="transparent"/>
        </svg>
        """
