// Screenshot carousels and full-screen in-page zoom/cycle lightbox.
// Without JavaScript, carousels still scroll sideways with CSS scroll-snap.
(function () {
  var lightbox = null;
  var pageFigures = [];
  var activeIndex = 0;
  var zoom = 1;
  var panX = 0;
  var panY = 0;
  var dragging = false;
  var movedDuringDrag = false;
  var startX = 0;
  var startY = 0;

  function setupCarousel(root) {
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
      return Math.round(track.scrollLeft / Math.max(track.clientWidth, 1));
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

  function ensureLightbox() {
    if (lightbox) return lightbox;
    var overlay = document.createElement("div");
    overlay.className = "shot-lightbox";
    overlay.hidden = true;
    overlay.setAttribute("role", "dialog");
    overlay.setAttribute("aria-modal", "true");
    overlay.setAttribute("aria-label", "Image viewer");

    var top = document.createElement("div");
    top.className = "shot-lightbox-top";
    var counter = document.createElement("span");
    counter.className = "shot-lightbox-counter";
    var hint = document.createElement("span");
    hint.className = "shot-lightbox-hint";
    hint.textContent = "Click image to zoom \u00b7 Drag when zoomed \u00b7 \u2190/\u2192 keys cycle page images \u00b7 Esc to close";
    var closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "shot-lightbox-close";
    closeBtn.setAttribute("aria-label", "Close viewer");
    closeBtn.textContent = "\u2715";
    top.appendChild(counter);
    top.appendChild(hint);
    top.appendChild(closeBtn);

    var stage = document.createElement("div");
    stage.className = "shot-lightbox-stage";
    var prevBtn = document.createElement("button");
    prevBtn.type = "button";
    prevBtn.className = "shot-lightbox-arrow prev";
    prevBtn.setAttribute("aria-label", "Previous image on this page");
    prevBtn.textContent = "\u2039";
    var img = document.createElement("img");
    img.className = "shot-lightbox-img";
    img.alt = "";
    img.draggable = false;
    var nextBtn = document.createElement("button");
    nextBtn.type = "button";
    nextBtn.className = "shot-lightbox-arrow next";
    nextBtn.setAttribute("aria-label", "Next image on this page");
    nextBtn.textContent = "\u203a";
    stage.appendChild(prevBtn);
    stage.appendChild(img);
    stage.appendChild(nextBtn);

    var footer = document.createElement("div");
    footer.className = "shot-lightbox-footer";
    var caption = document.createElement("div");
    caption.className = "shot-lightbox-caption";
    var controls = document.createElement("div");
    controls.className = "shot-lightbox-controls";
    var zoomLabel = document.createElement("span");
    zoomLabel.textContent = "100%";
    var slider = document.createElement("input");
    slider.type = "range";
    slider.className = "shot-lightbox-slider";
    slider.min = "100";
    slider.max = "400";
    slider.step = "10";
    slider.value = "100";
    slider.setAttribute("aria-label", "Zoom level");
    var resetBtn = document.createElement("button");
    resetBtn.type = "button";
    resetBtn.className = "shot-lightbox-reset";
    resetBtn.textContent = "Reset 100%";
    controls.appendChild(zoomLabel);
    controls.appendChild(slider);
    controls.appendChild(resetBtn);
    footer.appendChild(caption);
    footer.appendChild(controls);

    overlay.appendChild(top);
    overlay.appendChild(stage);
    overlay.appendChild(footer);
    document.body.appendChild(overlay);

    function applyTransform() {
      img.style.transform = "translate(" + panX + "px, " + panY + "px) scale(" + zoom + ")";
      img.classList.toggle("is-zoomed", zoom > 1.01);
      slider.value = String(Math.round(zoom * 100));
      zoomLabel.textContent = Math.round(zoom * 100) + "%";
    }

    function resetZoom() {
      zoom = 1;
      panX = 0;
      panY = 0;
      applyTransform();
    }

    function showAt(idx) {
      if (!pageFigures.length) return;
      activeIndex = (idx + pageFigures.length) % pageFigures.length;
      var fig = pageFigures[activeIndex];
      var srcImg = fig.querySelector("img");
      var figCap = fig.querySelector("figcaption");
      if (srcImg) {
        img.src = srcImg.src;
        img.alt = srcImg.alt || "";
      }
      caption.textContent = figCap ? figCap.textContent : (srcImg ? srcImg.alt : "");
      counter.textContent = "Image " + (activeIndex + 1) + " of " + pageFigures.length + " on this page";
      prevBtn.hidden = pageFigures.length < 2;
      nextBtn.hidden = pageFigures.length < 2;
      resetZoom();

      var track = fig.closest(".carousel-track");
      if (track) {
        var siblings = Array.prototype.slice.call(track.children);
        var sIdx = siblings.indexOf(fig);
        if (sIdx >= 0) {
          track.scrollTo({ left: sIdx * track.clientWidth, behavior: "smooth" });
        }
      }
    }

    function close() {
      overlay.hidden = true;
      resetZoom();
    }

    closeBtn.addEventListener("click", close);
    prevBtn.addEventListener("click", function (e) { e.stopPropagation(); showAt(activeIndex - 1); });
    nextBtn.addEventListener("click", function (e) { e.stopPropagation(); showAt(activeIndex + 1); });
    stage.addEventListener("click", function (e) {
      if (e.target === stage) close();
    });

    resetBtn.addEventListener("click", resetZoom);
    slider.addEventListener("input", function () {
      var nextZoom = parseInt(slider.value, 10) / 100;
      if (nextZoom <= 1.01) {
        resetZoom();
      } else {
        var ratio = nextZoom / zoom;
        panX *= ratio;
        panY *= ratio;
        zoom = nextZoom;
        applyTransform();
      }
    });

    img.addEventListener("mousedown", function (e) {
      if (e.button !== 0) return;
      e.preventDefault();
      dragging = true;
      movedDuringDrag = false;
      startX = e.clientX - panX;
      startY = e.clientY - panY;
      if (zoom > 1.01) img.classList.add("is-dragging");
    });

    window.addEventListener("mousemove", function (e) {
      if (!dragging) return;
      var dx = e.clientX - startX - panX;
      var dy = e.clientY - startY - panY;
      if (Math.abs(dx) > 4 || Math.abs(dy) > 4) movedDuringDrag = true;
      if (zoom > 1.01 && movedDuringDrag) {
        panX = e.clientX - startX;
        panY = e.clientY - startY;
        applyTransform();
      }
    });

    window.addEventListener("mouseup", function (e) {
      if (!dragging) return;
      dragging = false;
      img.classList.remove("is-dragging");
      if (!movedDuringDrag && e.target === img) {
        var rect = img.getBoundingClientRect();
        var cx = rect.left + rect.width / 2;
        var cy = rect.top + rect.height / 2;
        var offsetX = e.clientX - cx;
        var offsetY = e.clientY - cy;
        if (zoom < 2.0) {
          var targetZoom = 2.2;
          panX = (panX - offsetX) * (targetZoom / zoom) + offsetX * 0.15;
          panY = (panY - offsetY) * (targetZoom / zoom) + offsetY * 0.15;
          zoom = targetZoom;
          applyTransform();
        } else if (zoom < 3.2) {
          var targetZoom2 = 3.4;
          panX = (panX - offsetX) * (targetZoom2 / zoom);
          panY = (panY - offsetY) * (targetZoom2 / zoom);
          zoom = targetZoom2;
          applyTransform();
        } else {
          resetZoom();
        }
      }
    });

    window.addEventListener("keydown", function (e) {
      if (overlay.hidden) return;
      if (e.key === "Escape") {
        e.preventDefault();
        close();
      } else if (e.key === "ArrowLeft") {
        e.preventDefault();
        showAt(activeIndex - 1);
      } else if (e.key === "ArrowRight") {
        e.preventDefault();
        showAt(activeIndex + 1);
      }
    });

    lightbox = { overlay: overlay, showAt: showAt };
    return lightbox;
  }

  function setupFigures() {
    pageFigures = Array.prototype.slice.call(document.querySelectorAll(".md-typeset figure.shot"));
    pageFigures.forEach(function (fig, idx) {
      var img = fig.querySelector("img");
      if (!img || img.dataset.lightboxReady) return;
      img.dataset.lightboxReady = "1";
      img.addEventListener("click", function () {
        pageFigures = Array.prototype.slice.call(document.querySelectorAll(".md-typeset figure.shot"));
        var curIdx = pageFigures.indexOf(fig);
        var lb = ensureLightbox();
        lb.overlay.hidden = false;
        lb.showAt(curIdx >= 0 ? curIdx : idx);
      });
    });
  }

  function init() {
    document.querySelectorAll(".carousel").forEach(setupCarousel);
    setupFigures();
  }

  if (window.document$ && typeof window.document$.subscribe === "function") {
    window.document$.subscribe(init);
  } else {
    document.addEventListener("DOMContentLoaded", init);
  }
})();
