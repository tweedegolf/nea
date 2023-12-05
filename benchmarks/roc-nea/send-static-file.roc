app "nea-test"
    packages { pf: "platform/main.roc" }
    imports [ 
        pf.Request.{ Request },
        "/home/folkertdev/rust/nea/platform/Cargo.toml" as file : Str ]
    provides [main] to pf

main : Request -> Str
main = \_ -> file 
