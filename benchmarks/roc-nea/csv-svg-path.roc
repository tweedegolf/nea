app "nea-test"
    packages { pf: "platform/main.roc" }
    imports []
    provides [main] to pf

parseNum : Str -> U32
parseNum = \input ->
    when Str.toU32 input is
        Ok v -> v
        Err _ -> crash "invalid input"

main : Str -> Str
main = \input ->
    input 
    |> Str.split "\n" 
    |> List.map \line -> 
        when Str.split line ", " is
            [ xStr, yStr ] -> ( parseNum xStr, parseNum yStr ) 
            _ -> crash "invalid input"
    |> List.walk "M 0 0 L" \accum, (x, y) -> "\(accum)\(Num.toStr x) \(Num.toStr y) "
    |> \d -> 
        """
        <svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
          <path d="\(d)" stroke="black" fill="transparent"/>
        </svg>
        """
