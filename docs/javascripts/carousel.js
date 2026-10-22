// Adds previous/next buttons and a position counter to screenshot carousels.
// Without JavaScript the carousel still scrolls sideways with scroll snapping.
(function () {
  function setup(root) {
    if (root.dataset.ready) return;
    root.dataset.ready = "1";
    var track = root.querySelector(".carousel-track");
    if (!track) return;
    var slides = track.children;
    if (slides.length < 2) return;
    var nav = document.createElement("div");
    nav.className = "carousel-nav";
    var prev = document.createElement("button");
    prev.type = "button";
    prev.textContent = "\u2039 Previous";
    var count = document.createElement("span");
    var next = document.createElement("button");
    next.type = "button";
    next.textContent = "Next \u203a";
    nav.appendChild(prev);
    nav.appendChild(count);
    nav.appendChild(next);
    root.appendChild(nav);
    function index() {
      return Math.round(track.scrollLeft / track.clientWidth);
    }
    function update() {
      count.textContent = index() + 1 + " / " + slides.length;
    }
    function go(delta) {
      var i = (index() + delta + slides.length) % slides.length;
      track.scrollTo({ left: i * track.clientWidth, behavior: "smooth" });
    }
    prev.addEventListener("click", function () { go(-1); });
    next.addEventListener("click", function () { go(1); });
    track.addEventListener("scroll", function () { window.requestAnimationFrame(update); });
    update();
  }
  function init() {
    document.querySelectorAll(".carousel").forEach(setup);
  }
  if (window.document$ && typeof window.document$.subscribe === "function") {
    window.document$.subscribe(init);
  } else {
    document.addEventListener("DOMContentLoaded", init);
  }
})();
