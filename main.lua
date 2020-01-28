return function()
    coroutine.yield(forkio(function()
        print("[forked.]")
        while true do
            coroutine.yield(sleep(1))
            print("[.]")
        end
    end))
    print("hello,")
    coroutine.yield(sleep(3))
    print("world")

    html = coroutine.yield(get("http://localhost/"))
    print(html)

    job = coroutine.yield(forkio(function()
        print("{forked.}")
        coroutine.yield(sleep(1))
        print("{finished}")
        return 42
    end))
    r = coroutine.yield(job:wait())
    print(r)
    r = coroutine.yield(job:wait())
    print(r)
end
