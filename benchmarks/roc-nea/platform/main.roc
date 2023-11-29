platform "nea"
    requires {} { main : Request -> Str }
    exposes []
    packages {}
    imports [Request.{ Request }]
    provides [mainForHost]

mainForHost : Request -> Str
mainForHost = \request  -> main request
