#!/usr/bin/env python3
"""Writes a folder of made-up Serilog text logs to try LogLens on.

    python tools/sample_logs.py <folder> [--entries 60000] [--days 3] [--follow]

Three "instances" write rolling daily files, the way a load-balanced API does. With --follow
the script keeps appending a few entries per second until interrupted, for trying live tail.
Everything in the output is invented.
"""
import argparse
import datetime as dt
import json
import math
import pathlib
import random
import time

FRAMEWORK = [
    "   at lambda_method{n}(Closure, Object)",
    "   at Microsoft.AspNetCore.Mvc.Infrastructure.ActionMethodExecutor.TaskOfActionResultExecutor.Execute(ActionContext actionContext, IActionResultTypeMapper mapper, ObjectMethodExecutor executor, Object controller, Object[] arguments)",
    "   at Microsoft.AspNetCore.Mvc.Infrastructure.ControllerActionInvoker.<InvokeActionMethodAsync>g__Logged|12_1(ControllerActionInvoker invoker)",
    "   at Microsoft.AspNetCore.Mvc.Infrastructure.ControllerActionInvoker.<InvokeNextActionFilterAsync>g__Awaited|10_0(ControllerActionInvoker invoker, Task lastTask, State next, Scope scope, Object state, Boolean isCompleted)",
    "   at Microsoft.AspNetCore.Mvc.Infrastructure.ControllerActionInvoker.Rethrow(ActionExecutedContextSealed context)",
    "   at Microsoft.AspNetCore.Mvc.Infrastructure.ResourceInvoker.<InvokeAsync>g__Logged|17_1(ResourceInvoker invoker)",
    "   at Shop.Api.ErrorHandler.Invoke(HttpContext context) in C:\\build\\src\\Shop.Api\\ErrorHandler.cs:line 33",
]

# (message, exception line, throw-site frame, controller method)
FAILURES = [
    ("Unhandled exception for POST /v1/otp/send", "Shop.RateLimitedException: Too many attempts, try again in {n} seconds.",
     "Shop.Accounts.OtpSender.Send(String phone, String locale) in C:\\build\\src\\Shop.Accounts\\OtpSender.cs:line 58", "OtpController.Send"),
    ("Unhandled exception for POST /v1/otp/send", "Shop.RateLimitedException: الرجاء المحاولة بعد {n} ثانية.",
     "Shop.Accounts.OtpSender.Send(String phone, String locale) in C:\\build\\src\\Shop.Accounts\\OtpSender.cs:line 58", "OtpController.Send"),
    ("Unhandled exception for POST /v1/sign-in", "Shop.NotFoundException: No account matches this phone number.",
     "Shop.Accounts.SignInHandler.Handle(SignInRequest request) in C:\\build\\src\\Shop.Accounts\\SignInHandler.cs:line 97", "SignInController.Post"),
    ("Unhandled exception for POST /v2/cart/items", "Shop.ValidationException: This product needs a size before it can be added.",
     "Shop.Carts.CartValidator.CheckOptions(CartItem item) in C:\\build\\src\\Shop.Carts\\CartValidator.cs:line 212", "CartController.AddItem"),
    ("Unhandled exception for POST /v1/checkout", "Shop.PaymentDeclinedException: The card issuer declined the charge.",
     "Shop.Orders.Checkout.Charge(Order order) in C:\\build\\src\\Shop.Orders\\Checkout.cs:line 340", "CheckoutController.Post"),
    ("Payment webhook could not be applied", "System.ArgumentNullException: Value cannot be null. (Parameter 'source')",
     "Shop.Payments.WebhookHandler.Apply(WebhookEvent received) in C:\\build\\src\\Shop.Payments\\WebhookHandler.cs:line 76", "WebhookController.Receive"),
]
CACHE_DOWN = ("Cache read failed, treating it as a miss", "StackExchange.Redis.RedisConnectionException: No connection is active/available to service this operation: GET product:{n}",
              "Shop.Caching.RedisCache.Get(String key) in C:\\build\\src\\Shop.Caching\\RedisCache.cs:line 88", "ProductController.Details")
NOTES = ["", "", "", "", "no onions please", "ring the bell twice", "بدون بصل", "اتصل عند الوصول", "extra napkins 🙏"]
PATHS = ["/v2/addresses", "/v1/profile", "/favicon.ico", "/v1/orders/history"]


def failure(rng, spec):
    message, exception, site, controller = spec
    source = f"C:\\build\\src\\Shop.Api\\Controllers\\{controller.split('.')[0]}.cs"
    frames = [f"   at {site}", f"   at Shop.Api.Controllers.{controller}(Object body) in {source}:line {rng.randint(40, 600)}"]
    frames += [frame.format(n=rng.randint(1000, 199999)) for frame in FRAMEWORK]
    return "ERR", "\r\n".join([message, exception.format(n=rng.randint(1, 59))] + frames)


def entry(rng, mood):
    """Level and text of one entry. `mood` is 'normal', or the incident under way: 'cache' or 'geo'."""
    roll = rng.random()
    if mood == "cache" and roll < 0.55:
        return failure(rng, CACHE_DOWN)
    if mood == "geo" and roll < 0.6 or roll < 0.11:
        kind = "city" if mood == "geo" else rng.choice(["city", "city", "city", "slow", "token"])
        if kind == "city":
            return "WRN", f"Geocoder found no city for ({rng.uniform(40.4, 41.0):.5f}, {rng.uniform(-74.4, -73.6):.5f}); using the default delivery zone."
        if kind == "slow":
            return "WRN", f"Slow query ({rng.randint(900, 9000)} ms): Orders.ByCustomer customer={rng.randint(1000, 999999)}"
        return "WRN", f"Rejected a request without a bearer token: GET {rng.choice(PATHS)}"
    if roll < 0.14:
        return failure(rng, rng.choice(FAILURES))
    kind = rng.choices(["list", "cart", "baskets", "order", "payment", "push"], [30, 18, 12, 6, 4, 3])[0]
    if kind == "list":
        return "INF", f"Products.List category={rng.randint(1, 400)} page={rng.randint(1, 9)} took {rng.randint(3, 180)} ms"
    if kind == "cart":
        body = {"id": "1", "productId": rng.randint(100000, 9999999), "qty": rng.randint(1, 4), "note": rng.choice(NOTES),
                "memberIds": [rng.randint(10000, 19999) for _ in range(rng.randint(0, 2))],
                "lat": round(rng.uniform(40.4, 41.0), 6), "lng": round(rng.uniform(-74.4, -73.6), 6), "addressId": None}
        return "INF", "Cart request body: " + json.dumps(body, separators=(",", ":"))
    if kind == "baskets":
        return "INF", f"Loaded the basket list for courier {rng.randint(1, 9)}"
    if kind == "order":
        return "INF", f"Order placed | correlation={rng.getrandbits(128):032x} | customer={rng.randint(1000, 999999)} | order={rng.randint(700000, 799999)}"
    if kind == "payment":
        return "INF", f"Payment webhook received: status=settled order={rng.randint(700000, 799999)} amount={rng.uniform(1, 90):.2f} USD"
    payload = {"to": [str(rng.randint(100, 99999))], "title": "Your order is on its way", "data": {"kind": "order", "order": rng.randint(700000, 799999)}}
    return "INF", "Push request: POST https://push.example.com/v1/messages\r\n" + json.dumps(payload, indent=2).replace("\n", "\r\n")


def header(when):
    offset = when.strftime("%z")
    return f"{when:%Y-%m-%d %H:%M:%S}.{when.microsecond // 1000:03d} {offset[:3]}:{offset[3:]}"


def target(folder, when, instance):
    return folder / (f"api{when:%Y%m%d}" + (f"_{instance:03d}" if instance else "") + ".log")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("folder", type=pathlib.Path)
    parser.add_argument("--entries", type=int, default=60000)
    parser.add_argument("--days", type=float, default=3)
    parser.add_argument("--follow", action="store_true", help="keep appending new entries until interrupted")
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()
    rng = random.Random(args.seed)
    args.folder.mkdir(parents=True, exist_ok=True)

    now = dt.datetime.now().astimezone()
    span = args.days * 86400
    # Traffic follows the time of day; two incidents make the timeline worth looking at.
    times = []
    while len(times) < args.entries:
        age = rng.uniform(0, span)
        hour = ((now - dt.timedelta(seconds=age)).hour + 21) % 24
        if rng.random() < 0.12 + 0.88 * max(0.0, math.sin(hour / 24 * math.pi)) ** 2:
            times.append(age)
    cache = (span * 0.36, span * 0.36 + 1500)
    geo = (span * 0.71, span * 0.71 + 5400)
    for incident, extra in ((cache, args.entries // 60), (geo, args.entries // 25)):
        times += [rng.uniform(*incident) for _ in range(extra)]
    times.sort(reverse=True)

    files = {}
    for age in times:
        when = now - dt.timedelta(seconds=age)
        mood = "cache" if cache[0] <= age <= cache[1] else "geo" if geo[0] <= age <= geo[1] else "normal"
        level, text = entry(rng, mood)
        path = target(args.folder, when, rng.randrange(3))
        if path not in files:
            files[path] = open(path, "w", encoding="utf-8", newline="")
        files[path].write(f"{header(when)} [{level}] {text}\r\n")
    for handle in files.values():
        handle.close()
    print(f"wrote {len(times)} entries to {len(files)} files in {args.folder}")

    if not args.follow:
        return
    print("appending new entries, Ctrl+C to stop")
    try:
        while True:
            time.sleep(rng.uniform(0.15, 0.9))
            for _ in range(rng.randint(1, 4)):
                when = dt.datetime.now().astimezone()
                level, text = entry(rng, "normal")
                first, _, rest = text.partition("\r\n")
                with open(target(args.folder, when, rng.randrange(3)), "a", encoding="utf-8", newline="") as out:
                    out.write(f"{header(when)} [{level}] {first}")
                    if rest and rng.random() < 0.5:
                        # Now and then the rest of an entry lands a moment later, as it can in real life.
                        out.flush()
                        time.sleep(0.4)
                    out.write(("\r\n" + rest if rest else "") + "\r\n")
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
