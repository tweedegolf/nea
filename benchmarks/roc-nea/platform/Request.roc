interface Request
    exposes [Request]
    imports []

Request : { 
    body: Str,
    headers : List (Str, Str),
    method: Str,
    path: Str,
    version: Str
}
