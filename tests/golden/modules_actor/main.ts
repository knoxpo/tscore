import * as conf from "./conf.ts";
conf.bump();
conf.bump();
// the namespace crosses a realm boundary: mutable exports snapshot
const echo = actor(() => {
    return { read: (msg: { type: string, ns: { name: string, tick: number } }) => msg.ns.name + ":" + msg.ns.tick };
});
console.log(await echo.send({ type: "read", ns: conf }));
conf.bump();
console.log("here still live:", conf.tick);
echo.stop();
